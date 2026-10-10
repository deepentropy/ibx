use std::time::Instant;

use crate::bridge::{Event, SharedState};
use crate::config::chrono_free_timestamp;
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::protocol::tick_decoder;
use crate::protocol::tick_decoder::rtbar_entries;
use crate::types::{InstrumentId, ReqId, TbtType, PRICE_SCALE};
use crossbeam_channel::Sender;

use super::{HeartbeatState, emit, clone_for_event, find_body_after_tag, extract_raw_tag};
use super::pool::FixSink;

/// A head timestamp query with no answer for this long fails with
/// "Request Timed Out", as the reference (ibx#428).
const HEAD_TIMESTAMP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Error text of a server rejection of a historical ticks request (10187).
const HISTORICAL_TICKS_ERROR: &str = "Failed to request historical ticks";

/// Error text of a server rejection of a histogram request (10188).
const HISTOGRAM_ERROR: &str = "Failed to request histogram data";

pub(crate) struct HmdsState {
    pub(crate) next_tbt_req_id: u32,
    /// Tick-by-tick streams (ibx#404), one per contract, type and size
    /// filter, in the order they were made (ibx#455).
    pub(crate) tbt_subscriptions: Vec<TbtSub>,
    /// Stream ids whose stream is gone, per farm, with the field count of
    /// its type: their entries are skipped by it (ibx#404).
    pub(crate) tbt_ended: Vec<(super::pool::FarmId, u64, usize)>,
    /// Time range (Unix seconds) in which the field count guess for an
    /// entry of an unknown stream id takes a number as a time: from 5 s
    /// before the start, 3 days long, as the reference (ibx#404).
    pub(crate) tbt_guess_window: (i64, i64),
    pub(crate) next_hmds_query_id: u32,
    /// The counter of the real-time bar query ids (`realTime{n}`, the
    /// reference's `jextend.eG`), from 1.
    pub(crate) next_rtbar_id: u32,
    pub(crate) disconnected: bool,
    /// In-flight historical bar queries: (query_id, req_id). No deadline,
    /// as in the reference: its bar query (`jextend.j`, `hmdscore`) waits
    /// for the server's answer, an error or a cancel; only a head timestamp
    /// query times out (ibx#485; ibx#231 failed a bar request after 60 s
    /// with a text of ibx's own).
    pub(crate) pending_historical: Vec<(String, ReqId)>,
    /// In-flight head timestamp queries: (window id, req_id, deadline).
    pub(crate) pending_head_ts: Vec<(String, ReqId, Instant)>,
    /// Running numbers of the window ids of head timestamp, histogram and
    /// fundamentals queries (ibx#428).
    pub(crate) next_head_ts_window: u32,
    pub(crate) next_histogram_window: u32,
    pub(crate) next_fundamental_window: u32,
    /// A scanner parameters request is on the wire.
    pub(crate) pending_scanner_params: bool,
    /// Scanner parameters of this connection: one request, then every
    /// client request is answered from here (ibx#457).
    pub(crate) scanner_params: Option<String>,
    /// Client parameters requests waiting for the answer (ibx#457).
    pub(crate) scanner_params_waiting: u32,
    /// Row limit of each scan type, from the scanner parameters (ibx#456).
    pub(crate) scan_size_limits: std::collections::HashMap<String, u32>,
    /// Live scanner subscriptions, matched to results by their id (ibx#457).
    pub(crate) pending_scanner: Vec<ScannerSub>,
    /// The ids of `pending_scanner` in the order the waiting subscriptions
    /// go out when the scanner parameters arrive (ibx#513).
    pub(crate) scanner_table: crate::control::scanner::ScannerTable,
    /// News queries in flight, matched to replies by query id (ibx#459).
    pub(crate) pending_news: Vec<NewsQuery>,
    pub(crate) pending_articles: Vec<NewsQuery>,
    /// Number of the last news query; one counter for all news queries.
    pub(crate) next_news_query: u32,
    /// Session key of news queries, built at the first one (ibx#459).
    pub(crate) news_url_key: Option<String>,
    /// Fundamental data queries waiting for their reply (#434).
    pub(crate) pending_fundamental: Vec<PendingFundamental>,
    /// Reports received this session, by conId and report type: a request
    /// for one is answered from here with no query, as the reference
    /// does (#434).
    pub(crate) fundamental_cache: Vec<CachedReport>,
    /// In-flight histogram queries, summed over their frames until the
    /// last one (ibx#433).
    pub(crate) pending_histogram: Vec<PendingHistogram>,
    pub(crate) pending_schedule: Vec<(String, ReqId)>,
    /// Historical ticks queries waiting for their last frame: window id,
    /// request id, data name.
    pub(crate) pending_ticks: Vec<(String, ReqId, String)>,
    /// formatDate of the head timestamp requests waiting (ibx#431).
    pub(crate) head_ts_format: std::collections::HashMap<ReqId, i32>,
    /// The cache key of the head timestamp requests waiting (ibx#486).
    head_ts_keys: std::collections::HashMap<ReqId, HeadTsKey>,
    /// Head timestamps the farm gave, Unix seconds, by contract, route,
    /// data and RTH flag: a later request is answered from here with no
    /// query, as the reference's store (`hmdscore.store.b`, ibx#486).
    head_ts_cache: std::collections::HashMap<HeadTsKey, i64>,
    /// How the bars of each bar request are written (ibx#431), until its
    /// answer ended (keepUpToDate: until its cancel).
    pub(crate) bar_requests: Vec<BarRequestInfo>,
    /// keepUpToDate requests and their bar series (ibx#429).
    pub(crate) live_bars: Vec<LiveBars>,
    /// The last 5-second bar of each farm and ticker id: a keepUpToDate
    /// request whose history ends after it takes it then (ibx#429).
    pub(crate) router_bars: Vec<(super::pool::FarmId, u32, u32, crate::types::RealTimeBar)>,
    /// Real-time bar subscriptions: 5-second bars are routed by ticker id.
    pub(crate) rtbar_subs: Vec<RtBarSub>,
    /// Most real-time bar requests at once (ibx#454), from the logon.
    pub(crate) max_real_time_requests: u32,
    /// req_ids that should keep streaming after initial batch (keepUpToDate=True).
    pub(crate) keep_up_to_date_reqs: std::collections::HashSet<ReqId>,
    /// Bar requests answered from more than one server query (BID_ASK is a
    /// Bid query plus an Ask query, ibx#408). Each leg also has its own
    /// `pending_historical` entry; the bars of a leg are held here instead of
    /// being delivered, until every leg has finished.
    pub(crate) multi_leg: Vec<MultiLegBars>,
    /// Scanner results parked for contract-detail enrichment before dispatch.
    /// Drained by the engine top-level after each hmds.poll, then handed to
    /// `CcpState::start_scanner_enrichment`.
    pub(crate) cold_scanner_results: Vec<(ReqId, crate::control::scanner::ScannerResult)>,
    /// The historical routing table of the logon (#445); None until it
    /// came.
    pub(crate) routing: Option<crate::engine::routing::RoutingTable>,
    /// The farm the messages being handled came from (#445).
    pub(crate) rx_farm: super::pool::FarmId,
    /// The farm each bar query went to, when not the primary one (#445).
    pub(crate) query_farms: std::collections::HashMap<String, super::pool::FarmId>,
}

/// A tick-by-tick stream (ibx#404), as the reference's router: one per
/// contract, type and size filter, fed to every request on it (ibx#455).
/// Its query, the farm and stream id the acknowledgement gave, the
/// increments it gave, the running prices in ticks (the wire carries
/// deltas) and the sizes of its last entry.
#[derive(Debug, Clone)]
pub(crate) struct TbtSub {
    pub(crate) instrument: InstrumentId,
    pub(crate) con_id: i64,
    /// Window id of the query (a stream prefix and a session counter).
    pub(crate) window_id: String,
    /// The whole query id.
    pub(crate) query_id: String,
    pub(crate) tbt_type: TbtType,
    pub(crate) ignore_size: bool,
    /// The farm the query went to; the acknowledgement's farm once it came.
    pub(crate) farm: super::pool::FarmId,
    pub(crate) rt_ticker_id: Option<u64>,
    /// The query is out and not answered yet.
    pub(crate) query_pending: bool,
    /// The request whose query it is: it gets the past ticks and the
    /// query's error. None once it left.
    pub(crate) owner: Option<ReqId>,
    /// The requests on the stream, in the order they came.
    pub(crate) clients: Vec<ReqId>,
    pub(crate) min_tick: f64,
    pub(crate) size_min_tick: Option<f64>,
    pub(crate) last: i64,
    pub(crate) bid: i64,
    pub(crate) ask: i64,
    pub(crate) mid: i64,
    pub(crate) last_size: u64,
    pub(crate) bid_size: u64,
    pub(crate) ask_size: u64,
    /// Past ticks were asked and none came yet.
    pub(crate) history_wanted: bool,
    /// Live ticks are held until the last frame of the past ticks, then
    /// given in order, as the reference.
    pub(crate) holding: bool,
    pub(crate) held: Vec<TbtTick>,
    /// The cancel goes at this time, or at the next tick of the stream, as
    /// the reference does.
    pub(crate) cancel_at: Option<Instant>,
}

/// One tick of a stream, built when its entry came, given to every request
/// of the stream (ibx#455).
#[derive(Debug, Clone)]
pub(crate) enum TbtTick {
    Trade { price: i64, size: i64, time: u64, exchange: String, conditions: String, past_limit: bool, unreported: bool },
    Quote { bid: i64, ask: i64, bid_size: i64, ask_size: i64, time: u64, bid_past_low: bool, ask_past_high: bool },
    MidPoint { mid_point: i64, time: u64 },
}

impl TbtSub {
    /// A stream some request still uses.
    pub(crate) fn is_live(&self) -> bool {
        !self.clients.is_empty()
    }

    /// The stream holds a value: a price or a size of its last entry that
    /// is not 0. The reference gives no tick before (ibx#404).
    fn has_data(&self) -> bool {
        self.last != 0 || self.bid != 0 || self.ask != 0 || self.mid != 0
            || self.last_size != 0 || self.bid_size != 0 || self.ask_size != 0
    }

    fn layout(&self) -> tick_decoder::TbtLayout {
        let sized = self.size_min_tick.is_some();
        match self.tbt_type {
            TbtType::Last | TbtType::AllLast => tick_decoder::TbtLayout::Trade { sized },
            TbtType::BidAsk => tick_decoder::TbtLayout::BidAsk { sized },
            TbtType::MidPoint => tick_decoder::TbtLayout::MidPoint { sized },
        }
    }

    /// A price in ticks as a fixed-point price.
    fn price(&self, ticks: i64) -> i64 {
        (ticks as f64 * self.min_tick * PRICE_SCALE as f64).round() as i64
    }

    /// A size in size increments as a size.
    fn size(&self, raw: u64) -> i64 {
        match self.size_min_tick {
            Some(step) => (raw as f64 * step).round() as i64,
            None => raw as i64,
        }
    }
}

/// How long a tick-by-tick stream with no client left waits before its
/// cancel, as in the reference (ibx#404).
pub(crate) const TBT_CANCEL_DELAY: std::time::Duration = std::time::Duration::from_secs(15);

/// Error text of a failed tick-by-tick request (10189); the reason follows.
pub(crate) const TBT_REQUEST_ERROR: &str = "Failed to request tick-by-tick data.";

/// Give a tick to every request of its stream (ibx#455), with one event
/// for the tick.
fn deliver_tbt(sub: &TbtSub, tick: &TbtTick, shared: &SharedState, event_tx: &Option<Sender<Event>>) {
    let instrument = sub.instrument;
    for (i, &req_id) in sub.clients.iter().enumerate() {
        match tick {
            TbtTick::Trade { price, size, time, exchange, conditions, past_limit, unreported } => {
                let trade = crate::types::TbtTrade {
                    instrument, req_id, tbt_type: sub.tbt_type, price: *price, size: *size, timestamp: *time,
                    exchange: exchange.clone(), conditions: conditions.clone(),
                    past_limit: *past_limit, unreported: *unreported,
                };
                if i == 0 {
                    emit(event_tx, Event::TbtTrade(trade.clone()));
                }
                shared.market.push_tbt_trade(trade);
            }
            TbtTick::Quote { bid, ask, bid_size, ask_size, time, bid_past_low, ask_past_high } => {
                let quote = crate::types::TbtQuote {
                    instrument, req_id, bid: *bid, ask: *ask, bid_size: *bid_size, ask_size: *ask_size,
                    timestamp: *time, bid_past_low: *bid_past_low, ask_past_high: *ask_past_high,
                };
                if i == 0 {
                    emit(event_tx, Event::TbtQuote(quote));
                }
                shared.market.push_tbt_quote(quote);
            }
            TbtTick::MidPoint { mid_point, time } => {
                let mid = crate::types::TbtMidPoint { instrument, req_id, mid_point: *mid_point, timestamp: *time };
                if i == 0 {
                    emit(event_tx, Event::TbtMidPoint(mid));
                }
                shared.market.push_tbt_mid_point(mid);
            }
        }
    }
}

/// A fundamental data query waiting for its reply (#434).
#[derive(Debug, Clone)]
pub(crate) struct PendingFundamental {
    pub(crate) window_id: String,
    pub(crate) req_id: ReqId,
    pub(crate) con_id: i64,
    pub(crate) report: &'static str,
    /// The farm it was sent to, where its cancel goes.
    pub(crate) farm: super::pool::FarmId,
}

/// A fundamentals report kept for the session (#434).
#[derive(Debug, Clone)]
pub(crate) struct CachedReport {
    pub(crate) con_id: i64,
    pub(crate) report: &'static str,
    pub(crate) data: String,
    pub(crate) used: Instant,
}

/// Size of the report cache above which reports idle for a minute are
/// dropped, oldest first, and its hard limit (#434).
const REPORT_CACHE_COMPACT: usize = 100;
const REPORT_CACHE_MAX: usize = 200;
const REPORT_CACHE_IDLE: std::time::Duration = std::time::Duration::from_secs(60);

/// A real-time bar request (ibx#454): its query, or none when it joined
/// the 5-second router of another request for the same contract, data and
/// regular hours flag, as the reference.
#[derive(Debug, Clone)]
pub(crate) struct RtBarSub {
    /// Window id of the query; empty for a request that joined a router.
    pub(crate) query_id: String,
    pub(crate) req_id: ReqId,
    pub(crate) con_id: i64,
    /// Server data name (`Last`, `Bid`, `MidPoint`...).
    pub(crate) data: &'static str,
    pub(crate) use_rth: bool,
    /// Ticker id given by the server's acknowledgement.
    pub(crate) ticker_id: Option<u32>,
    pub(crate) min_tick: f64,
}

/// How the bars of a bar request are written (ibx#431): formatDate, bars
/// shorter than a day or not, the instrument zone (the contract's trading
/// hours zone when known, else the reply's zone), and the start and end
/// of the request in Unix seconds for historicalDataEnd.
#[derive(Debug, Clone)]
pub(crate) struct BarRequestInfo {
    pub(crate) req_id: ReqId,
    pub(crate) format_date: i32,
    pub(crate) intraday: bool,
    pub(crate) zone: Option<String>,
    pub(crate) start: i64,
    pub(crate) end: i64,
}

/// A keepUpToDate bar request (ibx#429): its query, the 5-second router
/// the server gave it, and its bar series, into which every 5-second bar
/// is merged once the history came.
#[derive(Debug, Clone)]
pub(crate) struct LiveBars {
    pub(crate) req_id: ReqId,
    /// Window id and whole id of the query.
    pub(crate) window_id: String,
    pub(crate) query_id: String,
    /// The farm the query went to; the router's ticker id is on it.
    pub(crate) farm: super::pool::FarmId,
    pub(crate) ticker_id: Option<u32>,
    pub(crate) min_tick: f64,
    pub(crate) bar_size: crate::control::historical::BarSize,
    /// Trades: volumes and counts are summed; other data keep them unset.
    pub(crate) trades: bool,
    pub(crate) series: Vec<crate::control::historical::SeriesBar>,
    pub(crate) sessions: Vec<(i64, i64)>,
    pub(crate) history_done: bool,
    /// Zone of the reply, for the dates of bars of a day or longer.
    pub(crate) reply_zone: String,
}

/// Most real-time bar requests when the logon gives no limit, as the
/// reference (ibx#454).
pub(crate) const DEFAULT_MAX_REAL_TIME_REQUESTS: u32 = 40;

/// Error text of a rejected real-time bar query (420).
const INVALID_REAL_TIME_QUERY: &str = "Invalid Real-time Query";

/// Error text of a second answer for a real-time bar request (421).
const INVALID_ROUTE: &str = "Invalid Route";

/// A histogram request in flight (ibx#428, ibx#433).
#[derive(Debug)]
pub(crate) struct PendingHistogram {
    pub(crate) window_id: String,
    pub(crate) req_id: ReqId,
    pub(crate) sum: crate::control::histogram::HistogramSum,
}

/// One live scanner subscription (ibx#457).
#[derive(Debug, Clone)]
pub(crate) struct ScannerSub {
    pub(crate) scan_id: String,
    pub(crate) req_id: ReqId,
    pub(crate) request: crate::control::scanner::ScannerSubscription,
    /// The subscribe message body, built once the scanner parameters are
    /// known (ibx#456), sent again after a reconnect.
    pub(crate) xml: Option<String>,
    /// The subscribe went out on the current connection.
    pub(crate) sent: bool,
}

/// One news query in flight (ibx#459). Requests with the same query text
/// share it, as the reference does, and all get the reply.
#[derive(Debug, Clone)]
pub(crate) struct NewsQuery {
    pub(crate) id: String,
    pub(crate) req_ids: Vec<ReqId>,
    pub(crate) query: String,
}

/// Scanner notices of the historical data link, as the reference words
/// them (ibx#457): the text of 165 with the notice after its colon
/// (`jextend.dt.e(String)` gives `d7.I.c(text)`, joined by
/// `jextend.ac.a(String,String)`; ibx#485).
const SCANNER_LINK_LOST: &str =
    "Historical Market Data Service query message:HMDS server disconnect occurred.  Attempting reconnection...";
const SCANNER_LINK_RESTORED: &str = "Historical Market Data Service query message:HMDS server connection was successful.";
const SCANNER_CONNECT_FAILED: &str =
    "Historical Market Data Service query message:HMDS connection attempt failed.  Connection will be re-attempted...";

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
    pub(crate) req_id: ReqId,
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

impl BarRequestInfo {
    /// The instrument zone: the contract's when known, else the reply's,
    /// else the machine zone.
    pub(crate) fn zone_or(&self, reply_zone: &str) -> String {
        match &self.zone {
            Some(z) => z.clone(),
            None if !reply_zone.is_empty() => reply_zone.to_string(),
            None => crate::gateway::machine_time_zone(),
        }
    }

    /// A bar time as the reference writes it (ibx#431).
    pub(crate) fn bar_time(&self, secs: i64, reply_zone: &str) -> String {
        let zone = self.zone_or(reply_zone);
        let day_zone = if reply_zone.is_empty() { zone.as_str() } else { reply_zone };
        crate::control::historical::bar_time(secs, self.format_date, self.intraday, &zone, day_zone)
    }
}

impl HmdsState {
    pub(crate) fn new() -> Self {
        Self {
            next_tbt_req_id: 1,
            tbt_subscriptions: Vec::new(),
            tbt_ended: Vec::new(),
            tbt_guess_window: {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
                (now - 5, now - 5 + 259_200)
            },
            next_hmds_query_id: 1000,
            next_rtbar_id: 0,
            disconnected: false,
            pending_historical: Vec::new(),
            pending_head_ts: Vec::new(),
            next_head_ts_window: 1,
            next_histogram_window: 0,
            next_fundamental_window: 1,
            pending_scanner_params: false,
            scanner_params: None,
            scanner_params_waiting: 0,
            scan_size_limits: std::collections::HashMap::new(),
            pending_scanner: Vec::new(),
            scanner_table: Default::default(),
            pending_news: Vec::new(),
            pending_articles: Vec::new(),
            next_news_query: 0,
            news_url_key: None,
            pending_fundamental: Vec::new(),
            fundamental_cache: Vec::new(),
            pending_histogram: Vec::new(),
            pending_schedule: Vec::new(),
            pending_ticks: Vec::new(),
            head_ts_format: std::collections::HashMap::new(),
            head_ts_keys: std::collections::HashMap::new(),
            head_ts_cache: std::collections::HashMap::new(),
            bar_requests: Vec::new(),
            live_bars: Vec::new(),
            router_bars: Vec::new(),
            rtbar_subs: Vec::new(),
            max_real_time_requests: DEFAULT_MAX_REAL_TIME_REQUESTS,
            keep_up_to_date_reqs: std::collections::HashSet::new(),
            multi_leg: Vec::new(),
            cold_scanner_results: Vec::new(),
            routing: None,
            rx_farm: super::pool::PRIMARY_HMDS,
            query_farms: std::collections::HashMap::new(),
        }
    }

    /// A live streaming listener on a historical farm (5-second bars,
    /// tick-by-tick, scanner): no historical farm is closed as dormant
    /// while one exists, as in the reference (#445).
    pub(crate) fn has_live_listener(&self) -> bool {
        !self.rtbar_subs.is_empty() || !self.live_bars.is_empty() || !self.tbt_subscriptions.is_empty() || !self.pending_scanner.is_empty()
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
                    // The head is cut by characters: a cut at byte 200
                    // panicked inside one that is not ASCII (ibx#488).
                    log::debug!(
                        "HMDS W xml head (len={}): {:?}",
                        xml_tag.len(),
                        xml_tag.chars().take(200).collect::<String>(),
                    );
                    if let Some(mut resp) = crate::control::historical::parse_bar_response(xml_tag) {
                        let wid = crate::control::historical::window_id(&resp.query_id);
                        if let Some(pos) = self.pending_historical.iter().position(|(qid, _)| qid == wid) {
                            let (_, req_id) = self.pending_historical[pos];
                            let is_complete = resp.is_complete;
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
                            // A live request keeps its series (ibx#429).
                            if let Some(live) = self.live_bars.iter_mut().find(|l| l.req_id == req_id) {
                                let (bars, sessions) = crate::control::historical::parse_series(xml_tag);
                                live.series.extend(bars);
                                live.sessions.extend(sessions);
                                live.history_done |= is_complete;
                                if live.reply_zone.is_empty() {
                                    live.reply_zone = resp.timezone.clone();
                                }
                            }
                            self.write_bar_times(req_id, &mut resp, is_complete);
                            // Clone only when someone is listening on the event
                            // channel — a bar batch is a deep copy (ibx#242).
                            let for_event = clone_for_event(event_tx, &resp);
                            shared.reference.push_historical_data(req_id, resp);
                            if let Some(data) = for_event {
                                emit(event_tx, Event::HistoricalData { req_id, data });
                            }
                            if is_complete && !self.keep_up_to_date_reqs.contains(&req_id) {
                                self.pending_historical.remove(pos);
                                self.bar_requests.retain(|b| b.req_id != req_id);
                            }
                            if is_complete {
                                self.replay_router_bar(req_id, shared);
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
                    else if let Some(mut resp) = crate::control::historical::parse_head_timestamp_response(xml_tag) {
                        let wid = reply_window_id(xml_tag);
                        if let Some(pos) = self.pending_head_ts.iter().position(|(q, _, _)| q == wid) {
                            let (_, req_id, _) = self.pending_head_ts.remove(pos);
                            // The time by formatDate, as the reference writes it (ibx#431).
                            let format_date = self.head_ts_format.remove(&req_id).unwrap_or(1);
                            let key = self.head_ts_keys.remove(&req_id);
                            if let Some(secs) = crate::control::historical::parse_server_time(&resp.head_timestamp) {
                                resp.head_timestamp = crate::control::historical::head_timestamp_text(secs, format_date);
                                // Kept for the next request; the later of two
                                // values stays (`store.b.a(a, long)@19-66`).
                                if let Some(key) = key {
                                    let kept = self.head_ts_cache.entry(key).or_insert(secs);
                                    *kept = (*kept).max(secs);
                                }
                            }
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
                            if let Some(frame) = crate::control::historical::parse_tick_response(xml_tag) {
                                // Every frame is delivered at once; the last one
                                // ends the request, as the reference (ibx#432).
                                if frame.done {
                                    self.pending_ticks.remove(pos);
                                }
                                if let Some(data) = frame.data {
                                    shared.reference.push_historical_ticks(req_id, data, what_to_show, frame.done);
                                }
                            }
                        } else if self.on_tbt_history(wid, xml_tag, shared, event_tx).is_none() {
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
                    else if xml_tag.contains("<ResultSetTickerId>") && xml_tag.contains("<rtTickerId>") {
                        self.on_tbt_ack(xml_tag, shared);
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
                        if let Some(pos) = self.rtbar_subs.iter().position(|s| !s.query_id.is_empty() && s.query_id == wid) {
                            matched = true;
                            if ticker_id == 0 {
                                let sub = self.rtbar_subs.remove(pos);
                                log::warn!("HMDS rtbar req_id={}: invalid ticker id {:?}", sub.req_id, ticker_id_str);
                                shared.reference.push_historical_error(sub.req_id, 420, INVALID_REAL_TIME_QUERY.to_string());
                            } else if self.rtbar_subs[pos].ticker_id.is_some() {
                                // An answer for a request already on its
                                // router: 421 and the request ends, its
                                // router cancelled when no request is left
                                // on it (ibx#454).
                                let req_id = self.rtbar_subs[pos].req_id;
                                log::warn!("HMDS rtbar req_id={}: second answer {:?}", req_id, ticker_id_str);
                                shared.reference.push_historical_error(req_id, 421, INVALID_ROUTE.to_string());
                                if let Some(tid) = self.end_rtbar(req_id) {
                                    self.send_historical_cancel(&tid.to_string(), hmds_conn, hb);
                                }
                            } else {
                                let sub = &mut self.rtbar_subs[pos];
                                sub.ticker_id = Some(ticker_id);
                                sub.min_tick = min_tick;
                                log::info!("HMDS rtbar ticker_id={} min_tick={} for req_id={}", ticker_id, min_tick, sub.req_id);
                            }
                        }
                        if !matched {
                            // The 5-second router of a keepUpToDate request, on
                            // the farm the reply came from (ibx#429).
                            let farm = self.rx_farm;
                            if let Some(live) = self.live_bars.iter_mut().find(|l| l.window_id == wid) {
                                live.ticker_id = Some(ticker_id);
                                live.min_tick = min_tick;
                                live.farm = farm;
                                matched = true;
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
                        let mut released: Option<(ReqId, i32, String)> = None;
                        if let Some(qid) = &query_id {
                            let wid = crate::control::historical::window_id(qid);
                            if let Some(pos) = self.pending_historical.iter().position(|(q, _)| q == wid) {
                                let (leg_qid, req_id) = self.pending_historical.remove(pos);
                                self.keep_up_to_date_reqs.remove(&req_id);
                                self.live_bars.retain(|l| l.req_id != req_id);
                                self.bar_requests.retain(|b| b.req_id != req_id);
                                self.on_leg_failed(req_id, &leg_qid);
                                released = Some((req_id, 162, historical_service_error(&error_msg)));
                            } else if let Some(pos) = self.pending_head_ts.iter().position(|(q, _, _)| q == wid) {
                                let (_, req_id, _) = self.pending_head_ts.remove(pos);
                                self.head_ts_format.remove(&req_id);
                                self.head_ts_keys.remove(&req_id);
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
                            } else if let Some(pos) = self.rtbar_subs.iter().position(|s| s.query_id == wid) {
                                // A rejected real-time bar query: 420 with the server
                                // text, and the request ends (ibx#454).
                                let req_id = self.rtbar_subs.remove(pos).req_id;
                                released = Some((req_id, 420, crate::control::historical::join_error_text(INVALID_REAL_TIME_QUERY, &error_msg)));
                            } else if let Some(pos) = self.pending_scanner.iter().position(|s| s.scan_id == *qid) {
                                let req_id = self.drop_scanner(pos).req_id;
                                released = Some((req_id, 162, error_msg.clone()));
                            } else if let Some(pos) = self.tbt_subscriptions.iter().position(|s| s.window_id == wid && s.query_pending) {
                                // A refused tick-by-tick query ends its
                                // request with 10189 and the server's text
                                // (ibx#455).
                                self.end_tbt_query(pos, &error_msg, shared);
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
                                self.on_scanner_params(xml, hmds_conn, hb, shared);
                            }
                        }
                        "10005" => {
                            if let Some(xml) = parsed.get(&6118) {
                                if let Some(result) = crate::control::scanner::parse_scanner_response(xml) {
                                    self.on_scanner_result(result, shared);
                                }
                            }
                        }
                        "10032" => {
                            let raw_bytes = extract_raw_tag(msg, 96).unwrap_or_default();
                            if let Some(xml) = parsed.get(&6118) {
                                self.on_news_reply(xml, &raw_bytes, shared);
                            }
                        }
                        "10012" => {
                            if let Some(xml) = parsed.get(&6118) {
                                self.on_fundamental_reply(xml, msg, shared);
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
            "T" => {
                // The routing table, when it came after the logon (#445).
                if let Some(text) = crate::engine::routing::table_text(msg) {
                    let table = crate::engine::routing::RoutingTable::parse(&text, crate::engine::routing::TableKind::Historical);
                    log::info!("Historical routing table: {} farms", table.routes().len());
                    self.routing = Some(table);
                }
            }
            other => {
                // ibx#183 follow-up: was a silent _ => {} arm — log unhandled
                // msg_types so we can catch frames that bypass the W cascade
                // entirely (e.g. completion sentinels delivered as a different type).
                log::warn!("HMDS unhandled msg_type={:?} (msg_len={})", other, msg.len());
            }
        }
    }

    /// The acknowledgement of a tick-by-tick query (ibx#404): the stream
    /// id the server numbers per farm, the price and size increments. A
    /// stream id that is not above 0 ends the query's request with 10189,
    /// as the reference; the other requests on the stream wait. With past
    /// ticks asked and none come yet, live ticks are held until the past
    /// ticks end (ibx#455).
    fn on_tbt_ack(&mut self, xml: &str, shared: &SharedState) {
        use crate::control::historical::extract_xml_tag;
        let wid = reply_window_id(xml);
        let Some(pos) = self.tbt_subscriptions.iter().position(|s| s.window_id == wid && s.query_pending) else {
            log::info!("Tick-by-tick acknowledgement for no live query: {:?}", wid);
            return;
        };
        let rt: i64 = extract_xml_tag(xml, "rtTickerId").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        if rt <= 0 {
            self.end_tbt_query(pos, INVALID_REAL_TIME_QUERY, shared);
            return;
        }
        let farm = self.rx_farm;
        let sub = &mut self.tbt_subscriptions[pos];
        sub.query_pending = false;
        sub.rt_ticker_id = Some(rt as u64);
        sub.farm = farm;
        sub.min_tick = extract_xml_tag(xml, "minTick").and_then(|v| v.trim().parse().ok()).unwrap_or(0.01);
        sub.size_min_tick = extract_xml_tag(xml, "sizeMinTick").and_then(|v| v.trim().parse().ok());
        (sub.last, sub.bid, sub.ask, sub.mid) = (0, 0, 0, 0);
        (sub.last_size, sub.bid_size, sub.ask_size) = (0, 0, 0);
        sub.holding = sub.history_wanted;
        log::info!("Tick-by-tick {} {}: stream {} on farm {}, minTick {}, sizeMinTick {:?}",
            sub.tbt_type.as_str(), sub.window_id, rt, farm, sub.min_tick, sub.size_min_tick);
    }

    /// The query of a stream failed (ibx#455): its request ends with 10189
    /// and `text`, as the reference; the other requests on the stream stay
    /// and wait, and the next request for it sends a new query.
    fn end_tbt_query(&mut self, pos: usize, text: &str, shared: &SharedState) {
        let sub = &mut self.tbt_subscriptions[pos];
        sub.query_pending = false;
        log::warn!("Tick-by-tick {} query {} refused: {}", sub.tbt_type.as_str(), sub.query_id, text);
        if let Some(owner) = sub.owner.take() {
            sub.clients.retain(|r| *r != owner);
            shared.market.push_tbt_error(owner, 10189,
                crate::control::historical::join_error_text(TBT_REQUEST_ERROR, text));
        }
        if sub.clients.is_empty() && sub.rt_ticker_id.is_none() {
            self.tbt_subscriptions.remove(pos);
        }
    }

    /// A frame of the past ticks a tick-by-tick request asked for
    /// (ibx#455): given to the request whose query it is as historical
    /// ticks, read by the stream's type; after the last frame the live
    /// ticks held meanwhile go to every request of the stream. None when
    /// the window id is no stream's.
    fn on_tbt_history(&mut self, wid: &str, xml: &str, shared: &SharedState, event_tx: &Option<Sender<Event>>) -> Option<()> {
        let pos = self.tbt_subscriptions.iter().position(|s| s.window_id == wid)?;
        let sub = &mut self.tbt_subscriptions[pos];
        let Some(owner) = sub.owner else {
            log::info!("Past ticks of {} for a request that left: dropped", wid);
            return Some(());
        };
        sub.history_wanted = false;
        let frame = crate::control::historical::parse_tick_by_tick_history(xml, sub.tbt_type)?;
        if let Some(data) = frame.data {
            shared.reference.push_historical_ticks(owner, data, sub.tbt_type.as_str().to_string(), frame.done);
        }
        if frame.done && sub.holding {
            sub.holding = false;
            for tick in std::mem::take(&mut sub.held) {
                deliver_tbt(sub, &tick, shared, event_tx);
            }
        }
        Some(())
    }

    /// A tick-by-tick frame (ibx#404): each entry goes to the stream of its
    /// id on this farm and is read by that stream's type; prices are the
    /// running sum of the deltas times the price increment, sizes times the
    /// size increment. An entry of a stream that is gone is skipped by its
    /// type's field count, one of an id with no stream by the reference's
    /// guess. An entry of a stream waiting for its cancel makes the cancel
    /// go now and is not delivered; an entry of a stream with no request
    /// starts its cancel delay. Before the stream holds a value no tick is
    /// given, as the reference.
    fn handle_tbt_data(&mut self, msg: &[u8], shared: &SharedState, event_tx: &Option<Sender<Event>>) {
        let body = match find_body_after_tag(msg, b"35=E\x01") {
            Some(b) => b,
            None => return,
        };
        let sig_pos = body.windows(6).position(|w| w == b"\x018349=");
        let body = if let Some(pos) = sig_pos { &body[..pos] } else { body };
        let farm = self.rx_farm;
        let (subs, ended) = (&self.tbt_subscriptions, &self.tbt_ended);
        let frame = tick_decoder::decode_tbt_frame(body, |rt| {
            if let Some(s) = subs.iter().find(|s| s.farm == farm && s.rt_ticker_id == Some(rt)) {
                return tick_decoder::TbtEntryKind::Read(s.layout());
            }
            match ended.iter().find(|(f, r, _)| *f == farm && *r == rt) {
                Some(&(_, _, n)) => tick_decoder::TbtEntryKind::Skip(n),
                None => tick_decoder::TbtEntryKind::Guess,
            }
        }, self.tbt_guess_window);
        if !frame.skipped.is_empty() {
            log::info!("Tick-by-tick entries of no live stream on farm {} skipped: {:?}", farm, frame.skipped);
        }
        if frame.stop == tick_decoder::TbtStop::Malformed {
            log::warn!("Tick-by-tick frame malformed: {} entries kept", frame.entries.len());
        }
        let now = Instant::now();
        for entry in frame.entries {
            let Some(sub) = self.tbt_subscriptions.iter_mut()
                .find(|s| s.farm == farm && s.rt_ticker_id == Some(entry.rt_ticker_id)) else { continue };
            let tick = match entry.fields {
                tick_decoder::TbtFields::Trade { price_delta, attribs, size, exchange, conditions } => {
                    // A price out of range is dropped (ibx#272).
                    let Some(last) = sub.last.checked_add(price_delta) else {
                        log::warn!("Tick-by-tick trade out of range dropped");
                        continue;
                    };
                    (sub.last, sub.last_size) = (last, size);
                    TbtTick::Trade {
                        price: sub.price(last), size: sub.size(size), time: entry.time, exchange, conditions,
                        past_limit: attribs & 1 != 0, unreported: attribs & 2 != 0,
                    }
                }
                tick_decoder::TbtFields::BidAsk { bid_delta, ask_delta, attribs, bid_size, ask_size } => {
                    let (Some(bid), Some(ask)) = (sub.bid.checked_add(bid_delta), sub.ask.checked_add(ask_delta)) else {
                        log::warn!("Tick-by-tick quote out of range dropped");
                        continue;
                    };
                    (sub.bid, sub.ask, sub.bid_size, sub.ask_size) = (bid, ask, bid_size, ask_size);
                    TbtTick::Quote {
                        bid: sub.price(bid), ask: sub.price(ask), bid_size: sub.size(bid_size), ask_size: sub.size(ask_size),
                        time: entry.time, bid_past_low: attribs & 1 != 0, ask_past_high: attribs & 2 != 0,
                    }
                }
                tick_decoder::TbtFields::MidPoint { delta } => {
                    let Some(mid) = sub.mid.checked_add(delta) else {
                        log::warn!("Tick-by-tick midpoint out of range dropped");
                        continue;
                    };
                    sub.mid = mid;
                    TbtTick::MidPoint { mid_point: sub.price(mid), time: entry.time }
                }
            };
            if sub.cancel_at.is_some() {
                sub.cancel_at = Some(now);
                continue;
            }
            if sub.clients.is_empty() {
                sub.cancel_at = Some(now + TBT_CANCEL_DELAY);
            }
            if !sub.has_data() {
                continue;
            }
            if sub.holding {
                sub.held.push(tick);
                continue;
            }
            deliver_tbt(sub, &tick, shared, event_tx);
        }
    }

    /// A 5-second bar frame holds several bars (ibx#454). Each bar goes to
    /// the real-time bar request of its ticker id, and is merged into the
    /// series of every keepUpToDate request on that router, which gets the
    /// whole changed bar as an update (ibx#429). A bar for a ticker id
    /// with no request is answered with a cancel of that ticker id, as the
    /// reference.
    fn handle_rtbar_data(&mut self, msg: &[u8], shared: &SharedState, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let body = match find_body_after_tag(msg, b"35=G\x01") {
            Some(b) => b,
            None => return,
        };
        let sig_pos = body.windows(6).position(|w| w == b"\x018349=");
        let body = if let Some(pos) = sig_pos { &body[..pos] } else { body };
        let farm = self.rx_farm;
        let mut unknown: Vec<u32> = Vec::new();
        for (ticker_id, timestamp, payload) in rtbar_entries(body) {
            // Every request on the router gets the bar (ibx#454).
            let subs: Vec<(ReqId, f64)> = self.rtbar_subs.iter()
                .filter(|s| farm == super::pool::PRIMARY_HMDS && s.ticker_id == Some(ticker_id))
                .map(|s| (s.req_id, s.min_tick)).collect();
            // A bar of quotes has no volume, average price or trade count:
            // -1 each (captured 05/10/2026, MIDPOINT).
            let trades = self.rtbar_subs.iter().find(|s| s.ticker_id == Some(ticker_id))
                .is_none_or(|s| matches!(s.data, "Last" | "AggLast"));
            let live_tick = self.live_bars.iter()
                .find(|l| l.farm == farm && l.ticker_id == Some(ticker_id))
                .map(|l| l.min_tick);
            if subs.is_empty() && live_tick.is_none() {
                if !unknown.contains(&ticker_id) {
                    unknown.push(ticker_id);
                }
                continue;
            }
            if let Some(&(_, min_tick)) = subs.first() {
                if let Some(mut bar) = crate::control::historical::decode_bar_payload(payload, min_tick) {
                    bar.timestamp = timestamp;
                    if !trades {
                        (bar.volume, bar.wap, bar.count) = (-1.0, -1.0, -1);
                    }
                    for &(req_id, _) in &subs {
                        shared.market.push_real_time_bar(req_id, bar);
                    }
                }
            }
            if let Some(min_tick) = live_tick {
                if let Some(mut bar) = crate::control::historical::decode_bar_payload(payload, min_tick) {
                    bar.timestamp = timestamp;
                    self.router_bars.retain(|(f, t, _, _)| !(*f == farm && *t == ticker_id));
                    self.router_bars.push((farm, ticker_id, timestamp, bar));
                    let reqs: Vec<ReqId> = self.live_bars.iter()
                        .filter(|l| l.farm == farm && l.ticker_id == Some(ticker_id) && l.history_done)
                        .map(|l| l.req_id).collect();
                    for req_id in reqs {
                        self.update_live_bar(req_id, timestamp, &bar, shared);
                    }
                }
            }
        }
        for ticker_id in unknown {
            log::info!("HMDS 5-second bar for unknown ticker id {}: cancelling it", ticker_id);
            // The connection given is the one of the farm the bar came from.
            self.send_historical_cancel(&ticker_id.to_string(), hmds_conn, hb);
        }
    }

    /// Merge a 5-second bar into a keepUpToDate request's series and send
    /// the whole bar it changed as an update (ibx#429).
    fn update_live_bar(&mut self, req_id: ReqId, time: u32, bar: &crate::types::RealTimeBar, shared: &SharedState) {
        let Some(live) = self.live_bars.iter_mut().find(|l| l.req_id == req_id) else { return };
        let reply_zone = live.reply_zone.clone();
        let Some(i) = crate::control::historical::merge_five_seconds(
            &mut live.series, &live.sessions, live.bar_size, live.trades, time as i64, bar,
        ) else { return };
        let b = live.series[i].clone();
        let mut out = crate::control::historical::HistoricalBar {
            time: String::new(), open: b.open, high: b.high, low: b.low, close: b.close,
            volume: b.volume, wap: b.wap, count: b.count,
        };
        if let Some(info) = self.bar_requests.iter().find(|r| r.req_id == req_id) {
            out.time = info.bar_time(b.start, &reply_zone);
        }
        shared.reference.push_historical_update(req_id, out);
    }

    /// A keepUpToDate request whose history just ended takes the last
    /// 5-second bar its router gave before (ibx#429).
    fn replay_router_bar(&mut self, req_id: ReqId, shared: &SharedState) {
        let Some(live) = self.live_bars.iter().find(|l| l.req_id == req_id) else { return };
        let (farm, ticker) = (live.farm, live.ticker_id);
        let Some(&(_, _, time, bar)) = self.router_bars.iter().find(|(f, t, _, _)| *f == farm && Some(*t) == ticker) else { return };
        self.update_live_bar(req_id, time, &bar, shared);
    }

    /// Write the bar times of an answer as the reference writes them, and,
    /// at its end, the start and end of the request (ibx#431).
    fn write_bar_times(&self, req_id: ReqId, resp: &mut crate::control::historical::HistoricalResponse, is_complete: bool) {
        let Some(info) = self.bar_requests.iter().find(|r| r.req_id == req_id) else { return };
        for bar in &mut resp.bars {
            if let Some(secs) = crate::control::historical::parse_server_time(&bar.time) {
                bar.time = info.bar_time(secs, &resp.timezone);
            }
        }
        if is_complete {
            let zone = info.zone_or(&resp.timezone);
            resp.start = crate::control::historical::zoned_time(info.start, &zone);
            resp.end = crate::control::historical::zoned_time(info.end, &zone);
        }
    }

    /// A request for a stream that exists (same contract, type and size
    /// filter, answered or with its query out) joins it, and the stream's
    /// cancel, if one is waiting, is called off, as the reference's router
    /// (ibx#455). False when there is none: a query is needed.
    pub(crate) fn tbt_join(&mut self, req_id: ReqId, con_id: i64, instrument: InstrumentId, tbt_type: TbtType, ignore_size: bool) -> bool {
        let Some(sub) = self.tbt_subscriptions.iter_mut().find(|s| s.con_id == con_id
            && s.tbt_type == tbt_type && s.ignore_size == ignore_size
            && (s.rt_ticker_id.is_some() || s.query_pending)) else { return false };
        sub.clients.push(req_id);
        sub.instrument = instrument;
        sub.cancel_at = None;
        log::info!("Tick-by-tick request {} joins the {} stream {}", req_id, tbt_type.as_str(), sub.window_id);
        true
    }

    /// The reference's tick-by-tick limit (ibx#455): the contracts of the
    /// answered streams not waiting for their cancel (all farms, taken in
    /// their order, a stream waiting for its cancel removing its contract),
    /// plus the other contracts with a query out on `farm` (None: a farm
    /// not open yet, with none). A new request
    /// is refused when that reaches `limit` and its contract is not among
    /// the answered streams' ones.
    pub(crate) fn tbt_over_limit(&self, con_id: i64, farm: Option<super::pool::FarmId>, limit: usize) -> bool {
        let mut streams: Vec<i64> = Vec::new();
        for s in self.tbt_subscriptions.iter().filter(|s| s.rt_ticker_id.is_some()) {
            if s.cancel_at.is_some() {
                streams.retain(|c| *c != s.con_id);
            } else if !streams.contains(&s.con_id) {
                streams.push(s.con_id);
            }
        }
        let mut pending: Vec<i64> = Vec::new();
        for s in self.tbt_subscriptions.iter().filter(|s| s.query_pending && Some(s.farm) == farm && s.con_id != con_id) {
            if !pending.contains(&s.con_id) {
                pending.push(s.con_id);
            }
        }
        streams.len() + pending.len() >= limit && !streams.contains(&con_id)
    }

    /// Send a tick-by-tick query (ibx#404, ibx#455) to `sink`, the farm
    /// of its route (`farm`), as the reference writes it: the query id is
    /// a stream prefix with a session counter and the chart name of the
    /// contract and type; the contract's routing exchange (the
    /// high-precision book for a currency pair) and security type; each
    /// type under its own API name; the past ticks only when asked for; the
    /// size filter when asked for. `req_id` is the query's request; the
    /// requests left waiting on the stream after a failed query get it too.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_tbt_subscribe(
        &mut self,
        req_id: ReqId,
        con_id: i64,
        instrument: InstrumentId,
        symbol: &str,
        exchange: &str,
        sec_type: &str,
        tbt_type: TbtType,
        number_of_ticks: i32,
        ignore_size: bool,
        farm: super::pool::FarmId,
        sink: &mut dyn super::pool::FixSink,
        hb: &mut HeartbeatState,
    ) {
        let n = self.next_tbt_req_id;
        self.next_tbt_req_id += 1;
        let tbt_type_str = tbt_type.as_str();
        let sec_type = if sec_type.is_empty() { "STK" } else { sec_type };
        let api_exchange = if exchange.is_empty() { "SMART" } else { exchange };
        let mut wire_exchange = super::farm::routing_exchange(exchange, sec_type);
        if sec_type == "CASH" && wire_exchange == "IDEALPRO" {
            wire_exchange = "FXSUBPIP";
        }
        let window_id = format!("rtTicker{n}");
        let query_id = format!("{window_id};;{symbol}@{api_exchange}{tbt_type_str};;1;;true;;0;;U");
        let time_length = if number_of_ticks > 0 {
            format!("<timeLength>{number_of_ticks} t</timeLength>")
        } else {
            String::new()
        };
        // The query's last element, as the historical ticks query writes it.
        let filter = if ignore_size {
            "<Filter varName=\"filter\"><ignoreSize>true</ignoreSize></Filter>"
        } else {
            ""
        };
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <ListOfQueries>\
             <Query>\
             <id>{query_id}</id>\
             <contractID>{con_id}</contractID>\
             <exchange>{wire_exchange}</exchange>\
             <secType>{sec_type}</secType>\
             <type>TickData</type>\
             <data>{tbt_type_str}</data>\
             <refresh>ticks</refresh>\
             {time_length}\
             <source>API</source>\
             <needTotalValue>false</needTotalValue>\
             <wholeDays>false</wholeDays>\
             {filter}\
             </Query>\
             </ListOfQueries>"
        );
        let ts = chrono_free_timestamp();
        if sink.send_plain(&[(fix::TAG_MSG_TYPE, "W"), (fix::TAG_SENDING_TIME, &ts), (6118, &xml)]) {
            if farm == super::pool::PRIMARY_HMDS {
                hb.last_hmds_sent = Instant::now();
            }
            log::info!("Sent tick-by-tick query {} on farm {}: con_id={} type={}", window_id, farm, con_id, tbt_type_str);
        }
        // Requests left waiting on the stream by a failed query.
        let waiting = self.tbt_subscriptions.iter().position(|s| s.con_id == con_id
            && s.tbt_type == tbt_type && s.ignore_size == ignore_size && s.rt_ticker_id.is_none() && !s.query_pending);
        let mut clients = match waiting {
            Some(pos) => self.tbt_subscriptions.remove(pos).clients,
            None => Vec::new(),
        };
        clients.push(req_id);
        self.tbt_subscriptions.push(TbtSub {
            instrument, con_id, window_id, query_id, tbt_type, ignore_size, farm, rt_ticker_id: None,
            query_pending: true, owner: Some(req_id), clients,
            min_tick: 0.01, size_min_tick: None, last: 0, bid: 0, ask: 0, mid: 0,
            last_size: 0, bid_size: 0, ask_size: 0,
            history_wanted: number_of_ticks > 0, holding: false, held: Vec::new(), cancel_at: None,
        });
    }

    /// The request `req_id` left its tick-by-tick stream: once no request
    /// is left on an answered stream its cancel goes 15 s later, or at the
    /// stream's next tick (ibx#404). A stream not answered yet waits for
    /// its answer and its first tick, as the reference. The stream's
    /// instrument, None when the request has no stream.
    pub(crate) fn send_tbt_unsubscribe(&mut self, req_id: ReqId, now: Instant) -> Option<InstrumentId> {
        let sub = self.tbt_subscriptions.iter_mut().find(|s| s.clients.contains(&req_id))?;
        sub.clients.retain(|r| *r != req_id);
        if sub.owner == Some(req_id) {
            sub.owner = None;
        }
        if sub.clients.is_empty() && sub.rt_ticker_id.is_some() && sub.cancel_at.is_none() {
            sub.cancel_at = Some(now + TBT_CANCEL_DELAY);
        }
        Some(sub.instrument)
    }

    /// The cancels that are due: the farm and the cancel message of each
    /// stream id. The stream's id and field count are kept to skip its
    /// later entries.
    pub(crate) fn take_due_tbt_cancels(&mut self, now: Instant) -> Vec<(super::pool::FarmId, String)> {
        if self.tbt_subscriptions.iter().all(|s| s.cancel_at.is_none_or(|t| t > now)) {
            return Vec::new();
        }
        let mut out = Vec::new();
        let ended = &mut self.tbt_ended;
        self.tbt_subscriptions.retain(|s| {
            let Some(rt) = s.rt_ticker_id.filter(|_| s.cancel_at.is_some_and(|t| t <= now)) else {
                return true;
            };
            ended.retain(|(f, r, _)| !(*f == s.farm && *r == rt));
            ended.push((s.farm, rt, tick_decoder::tbt_field_count(s.layout())));
            out.push((s.farm, format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <ListOfCancelQueries><CancelQuery><id>rtTicker:{rt}</id></CancelQuery></ListOfCancelQueries>"
            )));
            false
        });
        out
    }

    /// Send a bar request to `hmds_conn`, the farm of its route (`farm`),
    /// one query per leg. `symbol` is the symbol of the chart name of a
    /// live query; `zone` the contract's trading hours zone when known. The
    /// end date goes in UTC; the start and end of the request are kept for
    /// historicalDataEnd (ibx#431). A keepUpToDate request keeps its bar
    /// series for the updates (ibx#429).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_historical_request_ex(
        &mut self,
        req_id: ReqId,
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
        hmds_conn: &mut dyn super::pool::FixSink,
        hb: &mut HeartbeatState,
        shared: &SharedState,
        include_expired: bool,
        format_date: i32,
        zone: Option<String>,
        farm: super::pool::FarmId,
    ) {
        // The reference checks, refused with its codes and texts and no
        // end (ibx#430, ibx#429). The client checks first; this is the
        // engine-side backstop for raw control-channel callers.
        let checked = match crate::control::historical::check_bar_request(
            end_date_time, duration, bar_size, what_to_show, None, keep_up_to_date, sec_type,
            shared.reference.backfill_years_limit(), include_expired,
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
        let machine_zone = crate::gateway::machine_time_zone();
        let now = crate::control::historical::now_secs();
        let end = crate::control::historical::parse_request_time(end_date_time, &machine_zone, now)
            .ok().flatten().map_or(now, |t| t.secs);
        self.bar_requests.retain(|b| b.req_id != req_id);
        self.bar_requests.push(BarRequestInfo {
            req_id,
            format_date,
            intraday: bs.is_intraday(),
            zone,
            start: crate::control::historical::duration_start(end, duration, &machine_zone),
            end,
        });
        let end_date_time = crate::control::historical::server_time(end);
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
                con_id,
                symbol: symbol.to_string(),
                sec_type: sec_type.to_string(),
                exchange: exchange.to_string(),
                data_type: leg_type,
                end_time: end_date_time.clone(),
                duration: duration.to_string(),
                bar_size: bs,
                use_rth,
                keep_up_to_date,
                include_expired,
            };

            let xml = crate::control::historical::build_query_xml(&req);
            let ts = chrono_free_timestamp();
            if hmds_conn.send_plain(&[
                (fix::TAG_MSG_TYPE, "W"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ]) {
                log::info!("Sent historical request: req_id={} con_id={} bar_size={} data={} live={}",
                    req_id, con_id, bar_size, leg_type.as_str(), keep_up_to_date);
                hb.last_hmds_sent = Instant::now();
            }
            if legs.len() > 1 {
                leg_states.push(BarLeg {
                    query_id: query_id.clone(),
                    data_type: leg_type,
                    state: LegState::Pending,
                });
            }
            if keep_up_to_date {
                self.keep_up_to_date_reqs.insert(req_id);
                self.live_bars.retain(|l| l.req_id != req_id);
                self.live_bars.push(LiveBars {
                    req_id,
                    window_id: query_id.clone(),
                    query_id: crate::control::historical::extract_xml_tag(&xml, "id").unwrap_or("").to_string(),
                    farm,
                    ticker_id: None,
                    min_tick: 0.01,
                    bar_size: bs,
                    trades: leg_type == crate::control::historical::BarDataType::Trades,
                    series: Vec::new(),
                    sessions: Vec::new(),
                    history_done: false,
                    reply_zone: String::new(),
                });
            }
            self.pending_historical.push((query_id, req_id));
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
        req_id: ReqId,
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
    fn on_leg_failed(&mut self, req_id: ReqId, leg_qid: &str) {
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
        let mut resp = crate::control::historical::HistoricalResponse {
            query_id: m.legs[0].query_id.clone(),
            timezone: m.timezone,
            bars,
            is_complete: true,
            ..Default::default()
        };
        self.write_bar_times(m.req_id, &mut resp, true);
        self.bar_requests.retain(|b| b.req_id != m.req_id);
        let for_event = clone_for_event(event_tx, &resp);
        shared.reference.push_historical_data(m.req_id, resp);
        if let Some(data) = for_event {
            emit(event_tx, Event::HistoricalData { req_id: m.req_id, data });
        }
    }

    /// cancelHistoricalData, as the reference answers it (ibx#431,
    /// ibx#429): a request that is not running gets 366; a running one gets
    /// 162 "API historical data query cancelled", its queries still waiting
    /// are cancelled by their ids, and the 5-second router of a
    /// keepUpToDate request is cancelled (`ticker:{id}`) once no other
    /// request uses it. Gives the farm and the body of each cancel to send.
    pub(crate) fn cancel_bar_request(&mut self, req_id: ReqId, shared: &SharedState) -> Vec<(super::pool::FarmId, String)> {
        let known = self.pending_historical.iter().any(|(_, r)| *r == req_id)
            || self.live_bars.iter().any(|l| l.req_id == req_id)
            || self.pending_schedule.iter().any(|(_, r)| *r == req_id);
        if !known {
            shared.reference.push_historical_error(req_id, 366,
                format!("No historical data query found for ticker id:{}", req_id));
            return Vec::new();
        }
        shared.reference.push_historical_error(req_id, 162,
            historical_service_error(&format!("API historical data query cancelled: {}", req_id)));
        shared.reference.purge_historical_updates(req_id);
        let mut out = Vec::new();
        let live = self.live_bars.iter().position(|l| l.req_id == req_id).map(|i| self.live_bars.remove(i));
        self.keep_up_to_date_reqs.remove(&req_id);
        self.multi_leg.retain(|m| m.req_id != req_id);
        self.bar_requests.retain(|b| b.req_id != req_id);
        let mut waiting = Vec::new();
        self.pending_historical.retain(|(qid, rid)| {
            if *rid == req_id {
                waiting.push(qid.clone());
                false
            } else {
                true
            }
        });
        for qid in waiting {
            match &live {
                Some(l) if l.history_done => {}
                Some(l) => out.push((l.farm, crate::control::historical::query_cancel_xml(&l.query_id))),
                None => {
                    let farm = self.query_farms.remove(&qid).unwrap_or(super::pool::PRIMARY_HMDS);
                    out.push((farm, crate::control::historical::query_cancel_xml(&qid)));
                }
            }
        }
        if let Some(pos) = self.pending_schedule.iter().position(|(_, r)| *r == req_id) {
            let (qid, _) = self.pending_schedule.remove(pos);
            out.push((super::pool::PRIMARY_HMDS, crate::control::historical::query_cancel_xml(&qid)));
        }
        if let Some(LiveBars { farm, ticker_id: Some(tid), .. }) = live {
            let shared_router = self.live_bars.iter().any(|l| l.farm == farm && l.ticker_id == Some(tid))
                || (farm == super::pool::PRIMARY_HMDS && self.rtbar_subs.iter().any(|s| s.ticker_id == Some(tid)));
            if !shared_router {
                self.router_bars.retain(|(f, t, _, _)| !(*f == farm && *t == tid));
                out.push((farm, ticker_cancel_xml(tid)));
            }
        }
        out
    }

    /// Send a cancel body built by [`Self::cancel_bar_request`].
    pub(crate) fn send_cancel_xml(xml: &str, sink: &mut dyn super::pool::FixSink, hb: &mut HeartbeatState) {
        let ts = chrono_free_timestamp();
        if sink.send_plain(&[(fix::TAG_MSG_TYPE, "Z"), (fix::TAG_SENDING_TIME, &ts), (6118, xml)]) {
            hb.last_hmds_sent = Instant::now();
        }
    }

    pub(crate) fn send_historical_cancel(&mut self, ticker_id: &str, hmds_conn: &mut dyn super::pool::FixSink, hb: &mut HeartbeatState) {
        Self::send_cancel_xml(&crate::control::historical::query_cancel_xml(&format!("ticker:{}", ticker_id)), hmds_conn, hb);
    }

    /// A head timestamp request the store answers (ibx#486): the reference
    /// answers a contract, route, data and RTH flag it was given before
    /// with the stored value and sends no query (`headtime.k.a(store.a,
    /// long)@0-86`; captured 02/10/2026, b1_431_hist_format: the second
    /// TRADES head timestamp of AAPL after the contract lookup, no 35=W).
    /// False when the store has no value for it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn head_timestamp_from_cache(&self, req_id: ReqId, con_id: i64, sec_type: &str, exchange: &str, what_to_show: &str,
        use_rth: bool, format_date: i32, shared: &SharedState, event_tx: &Option<Sender<Event>>) -> bool
    {
        let Some(&secs) = self.head_ts_cache.get(&head_ts_key(con_id, sec_type, exchange, what_to_show, use_rth)) else {
            return false;
        };
        let resp = crate::control::historical::HeadTimestampResponse {
            head_timestamp: crate::control::historical::head_timestamp_text(secs, format_date),
            timezone: String::new(),
        };
        log::info!("Head timestamp req_id={} con_id={} answered from the store", req_id, con_id);
        let for_event = clone_for_event(event_tx, &resp);
        shared.reference.push_head_timestamp(req_id, resp);
        if let Some(data) = for_event {
            emit(event_tx, Event::HeadTimestamp { req_id, data });
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_head_timestamp_request(&mut self, req_id: ReqId, con_id: i64, sec_type: &str, exchange: &str, what_to_show: &str, use_rth: bool, format_date: i32, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
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
            con_id,
            sec_type: sec_type.to_string(),
            exchange: exchange.to_string(),
            data_type,
            use_rth,
        };
        let xml = crate::control::historical::build_head_timestamp_xml(&req);
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = super::pool::send_plain_on(conn, &[
                (fix::TAG_MSG_TYPE, "W"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ]);
            log::info!("Sent head timestamp request: req_id={} con_id={}", req_id, con_id);
            hb.last_hmds_sent = Instant::now();
        }
        self.pending_head_ts.push((window_id, req_id, Instant::now() + HEAD_TIMESTAMP_TIMEOUT));
        self.head_ts_format.insert(req_id, format_date);
        self.head_ts_keys.insert(req_id, head_ts_key(con_id, sec_type, exchange, what_to_show, use_rth));
    }

    pub(crate) fn send_scanner_params_request(&mut self, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = super::pool::send_plain_on(conn, &[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (crate::control::scanner::TAG_SUB_PROTOCOL, "10001"),
            ]);
            self.pending_scanner_params = true;
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent scanner params request");
        }
    }

    /// A client asks for the scanner parameters (ibx#457): answered from
    /// the cache of this connection, else one request goes out and every
    /// waiting client gets its answer.
    pub(crate) fn req_scanner_params(&mut self, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
        if let Some(xml) = &self.scanner_params {
            shared.reference.push_scanner_params(xml.clone());
            return;
        }
        self.scanner_params_waiting += 1;
        if !self.pending_scanner_params {
            self.send_scanner_params_request(hmds_conn, hb);
        }
    }

    fn on_scanner_params(&mut self, xml: &str, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
        self.pending_scanner_params = false;
        for _ in 0..std::mem::take(&mut self.scanner_params_waiting) {
            shared.reference.push_scanner_params(xml.to_string());
        }
        self.scan_size_limits = crate::control::scanner::scan_size_limits(xml);
        self.scanner_params = Some(xml.to_string());
        self.send_waiting_scanners(hmds_conn, hb);
    }

    /// Send the subscriptions that are not on the wire yet. A subscription
    /// goes out only once the scanner parameters are known, as the
    /// reference does: its row count depends on the scan type's limit
    /// there (ibx#456).
    fn send_waiting_scanners(&mut self, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if self.scanner_params.is_none() {
            return;
        }
        // Several wait only for the parameters: they go out in the
        // reference's order, not the order of the requests (ibx#513).
        let waiting: Vec<usize> = self.scanner_table.ids()
            .filter_map(|id| self.pending_scanner.iter().position(|s| !s.sent && s.scan_id == id))
            .collect();
        for i in waiting {
            let sub = &mut self.pending_scanner[i];
            let xml = sub.xml.get_or_insert_with(|| {
                let limit = self.scan_size_limits.get(&sub.request.scan_code).copied();
                let max_items = crate::control::scanner::scanner_max_items(sub.request.number_of_rows, limit);
                crate::control::scanner::build_scanner_subscribe_xml(&sub.request, &sub.scan_id, max_items)
            });
            sub.sent = Self::send_scanner_xml(xml, hmds_conn, hb);
            if sub.sent {
                log::info!("Sent scanner subscribe: req_id={} scan_code={}", sub.req_id, sub.request.scan_code);
            }
        }
    }

    /// Running scanner sessions for the concurrent limit: the
    /// subscriptions, plus one for parameters requests in progress
    /// (ibx#457).
    pub(crate) fn scanner_sessions(&self) -> usize {
        self.pending_scanner.len() + usize::from(self.scanner_params_waiting > 0)
    }

    /// A result of a scanner subscription, matched by its id (ibx#457).
    fn on_scanner_result(&mut self, result: crate::control::scanner::ScannerResult, shared: &SharedState) {
        let Some(pos) = self.pending_scanner.iter().position(|s| s.scan_id == result.id) else {
            log::info!("Received scan msg after window closed: id={:?}", result.id);
            return;
        };
        let req_id = self.pending_scanner[pos].req_id;
        if !result.error_text.is_empty() {
            self.drop_scanner(pos);
            shared.reference.push_historical_error(req_id, 162, historical_service_error(&result.error_text));
            return;
        }
        if !result.warning_text.is_empty() {
            shared.reference.push_historical_error(req_id, 165,
                format!("Historical Market Data Service query message:{}", result.warning_text));
        }
        // The rows are cut to the asked count (ibx#456).
        let mut result = result;
        if let Ok(rows) = usize::try_from(self.pending_scanner[pos].request.number_of_rows) {
            if rows > 0 && result.entries.len() > rows {
                result.entries.truncate(rows);
                result.con_ids = result.entries.iter().map(|e| e.con_id).filter(|c| *c != 0).collect();
            }
        }
        // A result carries conIds only; the contract fields come from a
        // contract lookup on the auth connection. Results with conIds not
        // in the cache are parked for the engine to enrich before dispatch
        // (ibx#156).
        let any_cold = result.entries.iter().any(|e| {
            e.con_id != 0
                && shared.reference.get_contract(e.con_id).is_none()
        });
        if any_cold {
            self.cold_scanner_results.push((req_id, result));
        } else {
            shared.reference.push_scanner_data(req_id, result);
        }
    }

    fn send_scanner_xml(xml: &str, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) -> bool {
        let Some(conn) = hmds_conn.as_mut() else { return false };
        let ts = chrono_free_timestamp();
        let _ = super::pool::send_plain_on(conn, &[
            (fix::TAG_MSG_TYPE, "U"),
            (fix::TAG_SENDING_TIME, &ts),
            (6040, "10003"),
            (6118, xml),
        ]);
        hb.last_hmds_sent = Instant::now();
        true
    }

    /// Start a scanner subscription (ibx#456): sent at once when the
    /// scanner parameters are known, else after they arrive.
    pub(crate) fn send_scanner_subscribe(&mut self, req_id: ReqId, client_id: i64, request: crate::control::scanner::ScannerSubscription, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let scan_id = crate::control::scanner::scanner_subscription_id(client_id, req_id);
        self.scanner_table.insert(&scan_id);
        self.pending_scanner.push(ScannerSub { scan_id, req_id, request, xml: None, sent: false });
        if self.scanner_params.is_some() {
            self.send_waiting_scanners(hmds_conn, hb);
        } else if !self.pending_scanner_params {
            self.send_scanner_params_request(hmds_conn, hb);
        }
    }

    /// End the subscription at `pos`.
    fn drop_scanner(&mut self, pos: usize) -> ScannerSub {
        let sub = self.pending_scanner.remove(pos);
        self.scanner_table.remove(&sub.scan_id);
        sub
    }

    pub(crate) fn send_scanner_cancel(&mut self, scan_id: &str, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let xml = crate::control::scanner::build_scanner_cancel_xml(scan_id);
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = super::pool::send_plain_on(conn, &[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (6040, "10004"),
                (6118, &xml),
            ]);
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent scanner cancel: scan_id={}", scan_id);
        }
    }

    /// Cancel a scanner subscription as the reference does (ibx#457): the
    /// local 162 first, then the desubscribe when a subscribe went out; an
    /// unknown request id gets 365 and nothing is sent. Returns true when
    /// the request id was live.
    pub(crate) fn cancel_scanner(&mut self, req_id: ReqId, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) -> bool {
        let Some(pos) = self.pending_scanner.iter().position(|s| s.req_id == req_id) else {
            shared.reference.push_historical_error(req_id, 365,
                format!("No scanner subscription found for ticker id:{}", req_id));
            return false;
        };
        let sub = self.drop_scanner(pos);
        shared.reference.push_historical_error(req_id, 162,
            historical_service_error(&format!("API scanner subscription cancelled: {}", req_id)));
        if sub.sent {
            self.send_scanner_cancel(&sub.scan_id, hmds_conn, hb);
        }
        self.cold_scanner_results.retain(|(r, _)| *r != req_id);
        shared.reference.discard_scanner_data(req_id);
        true
    }

    /// The historical data link is down (ibx#457): every live scanner is
    /// told, and the parameters are asked again on the next connection.
    pub(crate) fn scanner_link_lost(&mut self, shared: &SharedState) {
        self.scanner_params = None;
        self.pending_scanner_params = false;
        for sub in &mut self.pending_scanner {
            sub.sent = false;
            shared.reference.push_historical_error(sub.req_id, 165, SCANNER_LINK_LOST.to_string());
        }
    }

    /// A reconnect attempt of the historical data link failed (ibx#457).
    pub(crate) fn scanner_connect_failed(&self, shared: &SharedState) {
        for sub in &self.pending_scanner {
            shared.reference.push_historical_error(sub.req_id, 165, SCANNER_CONNECT_FAILED.to_string());
        }
    }

    /// The historical data link is back (ibx#457): every live scanner is
    /// told and subscribed again; waiting parameters requests go out.
    pub(crate) fn scanner_link_restored(&mut self, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
        for i in 0..self.pending_scanner.len() {
            let req_id = self.pending_scanner[i].req_id;
            shared.reference.push_historical_error(req_id, 165, SCANNER_LINK_RESTORED.to_string());
            if let Some(xml) = &self.pending_scanner[i].xml {
                let sent = Self::send_scanner_xml(xml, hmds_conn, hb);
                self.pending_scanner[i].sent = sent;
            }
        }
        let waiting = self.scanner_params_waiting > 0 || self.pending_scanner.iter().any(|s| !s.sent);
        if waiting && self.scanner_params.is_none() && !self.pending_scanner_params {
            self.send_scanner_params_request(hmds_conn, hb);
        }
    }

    /// A news reply, matched to its query by id (ibx#459). A failure is
    /// 10173 for headlines and 10172 for an article, with no end message.
    fn on_news_reply(&mut self, xml: &str, raw: &[u8], shared: &SharedState) {
        let Some(id) = crate::control::news::parse_news_response_id(xml) else {
            log::warn!("News reply with no query id");
            return;
        };
        if let Some(pos) = self.pending_articles.iter().position(|q| q.id == id) {
            let query = self.pending_articles.remove(pos);
            let result = crate::control::news::parse_article_payload(raw);
            for req_id in query.req_ids {
                match &result {
                    Ok((atype, text)) => shared.reference.push_news_article(req_id, *atype, text.clone()),
                    Err(reason) => shared.reference.push_historical_error(req_id, 10172,
                        format!("Failed to request news article:{}", reason)),
                }
            }
        } else if let Some(pos) = self.pending_news.iter().position(|q| q.id == id) {
            let query = self.pending_news.remove(pos);
            let result = crate::control::news::parse_news_payload(raw);
            for req_id in query.req_ids {
                match &result {
                    Ok((headlines, has_more)) => shared.reference.push_historical_news(req_id, headlines.clone(), *has_more),
                    Err(reason) => shared.reference.push_historical_error(req_id, 10173,
                        format!("Failed to request historical news:{}", reason)),
                }
            }
        } else {
            log::warn!("News reply for no pending query: id={:?}", id);
        }
    }

    /// The session key of news queries, built once (ibx#459).
    fn news_url_key(&mut self, shared: &SharedState) -> String {
        self.news_url_key.get_or_insert_with(|| {
            let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs()).unwrap_or(0);
            crate::control::news::news_url_key(&shared.reference.news_sources(), epoch,
                rand::random_range(0..1_000_000_000u32))
        }).clone()
    }

    /// Send a news query, or join an equal one in flight (ibx#459).
    fn send_news_query(queries: &mut Vec<NewsQuery>, req_id: ReqId, id: String, xml: String,
                       hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let query = crate::control::news::news_query_text(&xml).to_string();
        if let Some(q) = queries.iter_mut().find(|q| q.query == query) {
            q.req_ids.push(req_id);
            return;
        }
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = super::pool::send_plain_on(conn, &[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (6040, "10030"),
                (6118, &xml),
            ]);
            hb.last_hmds_sent = Instant::now();
        }
        queries.push(NewsQuery { id, req_ids: vec![req_id], query });
    }

    pub(crate) fn send_historical_news_request(&mut self, req_id: ReqId, con_id: i64, provider_codes: &str, start_time: &str, end_time: &str, max_results: u32, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
        self.next_news_query += 1;
        let req = crate::control::news::HistoricalNewsRequest {
            query_id: self.next_news_query.to_string(),
            con_id,
            provider_codes: provider_codes.to_string(),
            start_time: start_time.to_string(),
            end_time: end_time.to_string(),
            max_results,
            subscribed: shared.reference.news_sources(),
            url_key: self.news_url_key(shared),
        };
        let (cmd, _) = crate::control::news::historical_news_command(&req);
        let id = crate::control::news::news_query_id(&req.query_id, cmd);
        let xml = crate::control::news::build_historical_news_xml(&req);
        Self::send_news_query(&mut self.pending_news, req_id, id, xml, hmds_conn, hb);
        log::info!("Sent historical news request: req_id={} con_id={}", req_id, con_id);
    }

    pub(crate) fn send_news_article_request(&mut self, req_id: ReqId, provider_code: &str, article_id: &str, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
        self.next_news_query += 1;
        let req = crate::control::news::NewsArticleRequest {
            query_id: self.next_news_query.to_string(),
            provider_code: provider_code.to_string(),
            article_id: article_id.to_string(),
            url_key: self.news_url_key(shared),
        };
        let id = crate::control::news::news_query_id(&req.query_id, "article_file");
        let xml = crate::control::news::build_article_request_xml(&req);
        Self::send_news_query(&mut self.pending_articles, req_id, id, xml, hmds_conn, hb);
        log::info!("Sent news article request: req_id={} article={}", req_id, article_id);
    }

    /// A fundamental data request (#434), as the reference handles it: a
    /// request id still waiting is refused with 322; a report received
    /// before for the contract and type is answered from memory; else the
    /// query goes to `sink`, the farm of the fundamentals route (`farm`).
    /// The report type is the reference's (an unknown name is asked with
    /// no type); the query id is the provider and a session counter.
    pub(crate) fn send_fundamental_data_request(&mut self, req_id: ReqId, con_id: i64, report_type: &str, farm: super::pool::FarmId, sink: &mut dyn super::pool::FixSink, hb: &mut HeartbeatState, shared: &SharedState) {
        if self.pending_fundamental.iter().any(|p| p.req_id == req_id) {
            shared.reference.push_historical_error(req_id, 322, "Error processing request.-'bL' : cause - Duplicate ticker id".into());
            return;
        }
        let rt = crate::control::fundamental::ReportType::from_api(report_type);
        if let Some(cached) = self.fundamental_cache.iter_mut().find(|c| c.con_id == con_id && c.report == rt.wire_name) {
            cached.used = Instant::now();
            log::info!("Fundamentals {} for con_id {} answered from memory (req_id={})", rt.wire_name, con_id, req_id);
            shared.reference.push_fundamental_data(req_id, cached.data.clone());
            return;
        }
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
        let ts = chrono_free_timestamp();
        if sink.send_plain(&[
            (fix::TAG_MSG_TYPE, "U"),
            (fix::TAG_SENDING_TIME, &ts),
            (6040, "10010"),
            (6118, &xml),
        ]) {
            if farm == super::pool::PRIMARY_HMDS {
                hb.last_hmds_sent = Instant::now();
            }
            log::info!("Sent fundamental data request on farm {}: req_id={} con_id={} type={:?}", farm, req_id, con_id, rt.wire_name);
        }
        self.pending_fundamental.push(PendingFundamental { window_id, req_id, con_id, report: rt.wire_name, farm });
    }

    /// Cancel a fundamental data request: while its query waits for the
    /// reply, a cancel of the query goes to its farm; after the reply, or
    /// for an unknown id, nothing is sent (#434). Returns the farm and the
    /// cancel message to send.
    pub(crate) fn cancel_fundamental(&mut self, req_id: ReqId) -> Option<(super::pool::FarmId, String)> {
        let pos = self.pending_fundamental.iter().position(|p| p.req_id == req_id)?;
        let p = self.pending_fundamental.remove(pos);
        Some((p.farm, crate::control::fundamental::build_fundamental_cancel_xml(&p.window_id)))
    }

    /// The reply of a fundamentals query (#434): the report is the gzip
    /// payload cut from the raw message with its length, inflated; a server
    /// error text, an empty payload or a payload that does not inflate is
    /// error 430 with its cause, never data.
    fn on_fundamental_reply(&mut self, xml: &str, raw: &[u8], shared: &SharedState) {
        use crate::control::fundamental as f;
        let wid = f::parse_fundamental_response_id(xml)
            .map(|id| crate::control::historical::window_id(&id).to_string())
            .unwrap_or_default();
        let Some(pos) = self.pending_fundamental.iter().position(|p| p.window_id == wid) else {
            log::warn!("HMDS fundamentals reply for no pending request: id={:?}", wid);
            return;
        };
        let p = self.pending_fundamental.remove(pos);
        let fail = |cause: &str| {
            log::warn!("Fundamentals req_id={} failed: {}", p.req_id, cause);
            shared.reference.push_historical_error(p.req_id, 430, format!("{}{}", f::FUNDAMENTALS_NOT_AVAILABLE, cause));
        };
        if let Some(text) = f::fundamental_error_text(xml) {
            return fail(&text);
        }
        let payload = super::extract_raw_tag(raw, 96).unwrap_or_default();
        if payload.is_empty() {
            return fail("Query failed");
        }
        let Some(data) = f::decompress_fundamental_data(&payload) else {
            return fail("Query failed");
        };
        self.cache_report(p.con_id, p.report, &data);
        shared.reference.push_fundamental_data(p.req_id, data);
    }

    fn cache_report(&mut self, con_id: i64, report: &'static str, data: &str) {
        let now = Instant::now();
        self.fundamental_cache.retain(|c| !(c.con_id == con_id && c.report == report));
        self.fundamental_cache.push(CachedReport { con_id, report, data: data.to_string(), used: now });
        if self.fundamental_cache.len() > REPORT_CACHE_COMPACT {
            // Oldest first: drop those idle for a minute down to the
            // compaction size, then anything above the hard limit.
            self.fundamental_cache.sort_by_key(|c| c.used);
            let mut excess = self.fundamental_cache.len() - REPORT_CACHE_COMPACT;
            self.fundamental_cache.retain(|c| {
                if excess > 0 && now.duration_since(c.used) > REPORT_CACHE_IDLE {
                    excess -= 1;
                    false
                } else {
                    true
                }
            });
            while self.fundamental_cache.len() > REPORT_CACHE_MAX {
                self.fundamental_cache.remove(0);
            }
        }
    }

    pub(crate) fn send_histogram_request(&mut self, req_id: ReqId, con_id: i64, sec_type: &str, exchange: &str, use_rth: bool, period: &str, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
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
            let _ = super::pool::send_plain_on(conn, &[
                (fix::TAG_MSG_TYPE, "W"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ]);
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent histogram request: req_id={} con_id={}", req_id, con_id);
        }
        // No deadline, as in the reference (ibx#485).
        self.pending_histogram.push(PendingHistogram { window_id, req_id, sum: Default::default() });
    }

    /// Send a historical ticks query (ibx#432) to `sink`, the farm of its
    /// route (`farm`), as the reference does: the local checks again
    /// (the client sent their warnings and refusals), a request id still
    /// waiting gets 102, a date in a zone that is not UTC, the machine zone
    /// or the contract's zone (`zone`, when known) gets 10314; else the
    /// query goes out, and its frames are matched by window id.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_historical_ticks_request(
        &mut self, req_id: ReqId, con_id: i64, symbol: &str, sec_type: &str, exchange: &str,
        start_date_time: &str, end_date_time: &str, number_of_ticks: i32, what_to_show: &str,
        use_rth: bool, ignore_size: bool, zone: Option<&str>, farm: super::pool::FarmId,
        sink: &mut dyn super::pool::FixSink, hb: &mut HeartbeatState, shared: &SharedState,
    ) {
        use crate::control::historical as h;
        let machine_zone = crate::gateway::machine_time_zone();
        let now = h::now_secs();
        let (_, checked) = h::check_ticks_request(start_date_time, end_date_time, number_of_ticks, what_to_show,
            ignore_size, sec_type, exchange, &machine_zone, now);
        let checked = match checked {
            Ok(c) => c,
            Err((code, text)) => {
                shared.reference.push_historical_error(req_id, code, text);
                return;
            }
        };
        if self.pending_ticks.iter().any(|(_, r, _)| *r == req_id) {
            shared.reference.push_historical_error(req_id, 102, "Duplicate ticker id".to_string());
            return;
        }
        let zone_ok = |t: &Option<h::RequestTime>| match t.as_ref().and_then(|t| t.zone.as_deref()) {
            None => true,
            Some(z) => z.eq_ignore_ascii_case("UTC") || z == machine_zone || zone.is_none_or(|c| c == z),
        };
        if !zone_ok(&checked.start) {
            shared.reference.push_historical_error(req_id, 10314,
                h::INVALID_END_DATE.replacen("End Date/Time", "Start Date/Time", 1));
            return;
        }
        if !zone_ok(&checked.end) {
            shared.reference.push_historical_error(req_id, 10314, h::INVALID_END_DATE.to_string());
            return;
        }
        let qid = self.next_hmds_query_id;
        self.next_hmds_query_id += 1;
        let query = h::TickQuery {
            window_id: format!("tk_{}", qid),
            symbol: symbol.to_string(),
            con_id,
            sec_type: sec_type.to_string(),
            exchange: exchange.to_string(),
            source: checked.source,
            start: checked.start.as_ref().map(|t| t.secs),
            end: checked.end.as_ref().map(|t| t.secs),
            number_of_ticks,
            use_rth,
            ignore_size_filter: checked.ignore_size_filter,
        };
        let xml = h::build_tick_query_xml(&query, now);
        let ts = chrono_free_timestamp();
        if sink.send_plain(&[
            (fix::TAG_MSG_TYPE, "W"),
            (fix::TAG_SENDING_TIME, &ts),
            (6118, &xml),
        ]) {
            if farm == super::pool::PRIMARY_HMDS {
                hb.last_hmds_sent = Instant::now();
            }
            log::info!("Sent historical ticks request on farm {}: req_id={} con_id={} what={}", farm, req_id, con_id, what_to_show);
        }
        self.pending_ticks.push((query.window_id, req_id, checked.source.data().to_string()));
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_realtime_bar_subscribe(&mut self, req_id: ReqId, con_id: i64, sec_type: &str, exchange: &str, symbol: &str, what_to_show: &str, use_rth: bool, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
        // The reference checks, in its order (ibx#454): whatToShow (321),
        // a request id already streaming (102), the request limit (456).
        if crate::control::historical::realtime_bar_data(what_to_show).is_none() {
            log::error!("rtbar req_id={}: whatToShow {:?} refused", req_id, what_to_show);
            shared.reference.push_historical_error(req_id, 321,
                "Error validating request.-'bS' : cause - What to show field is missing or incorrect.".to_string());
            return;
        }
        if self.rtbar_subs.iter().any(|s| s.req_id == req_id) {
            shared.reference.push_historical_error(req_id, 102, "Duplicate ticker id".to_string());
            return;
        }
        let data = crate::control::historical::realtime_bar_data(what_to_show).unwrap_or("Last");
        // The router of an answered request for the same contract, data and
        // regular hours flag (ibx#454).
        let router = self.rtbar_subs.iter()
            .find(|s| s.ticker_id.is_some() && s.con_id == con_id && s.data == data && s.use_rth == use_rth)
            .map(|s| (s.ticker_id, s.min_tick));
        // The limit counts the 5-second routers of the farm, one more for a
        // new one, as the reference.
        let mut routers: Vec<u32> = self.rtbar_subs.iter().filter_map(|s| s.ticker_id)
            .chain(self.live_bars.iter().filter(|l| l.farm == super::pool::PRIMARY_HMDS).filter_map(|l| l.ticker_id))
            .collect();
        routers.sort_unstable();
        routers.dedup();
        let wanted = routers.len() as u64 + u64::from(router.is_none());
        if wanted > self.max_real_time_requests as u64 {
            log::warn!("rtbar req_id={}: {} routers of {} allowed", req_id, routers.len(), self.max_real_time_requests);
            shared.reference.push_historical_error(req_id, 456,
                "Max number of real time requests has been reached".to_string());
            return;
        }
        if let Some((ticker_id, min_tick)) = router {
            log::info!("rtbar req_id={} joins the router of ticker id {:?}", req_id, ticker_id);
            self.rtbar_subs.push(RtBarSub { query_id: String::new(), req_id, con_id, data, use_rth, ticker_id, min_tick });
            return;
        }
        self.next_rtbar_id += 1;
        let query_id = format!("realTime{}", self.next_rtbar_id);
        let full_id = crate::control::historical::realtime_bar_query_id(&query_id, symbol, exchange, what_to_show);
        let xml = crate::control::historical::build_realtime_bar_xml(&full_id, con_id, sec_type, exchange, what_to_show, use_rth);
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = super::pool::send_plain_on(conn, &[
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
            con_id,
            data,
            use_rth,
            ticker_id: None,
            min_tick: 0.01,
        });
    }

    /// End the real-time bar request `req_id` (ibx#454): the ticker id to
    /// cancel when no request (real-time bars or keepUpToDate) is left on
    /// its router. A request with no answer yet has nothing to cancel, as
    /// the reference.
    pub(crate) fn end_rtbar(&mut self, req_id: ReqId) -> Option<u32> {
        let pos = self.rtbar_subs.iter().position(|s| s.req_id == req_id)?;
        let tid = self.rtbar_subs.remove(pos).ticker_id?;
        let shared_router = self.rtbar_subs.iter().any(|s| s.ticker_id == Some(tid))
            || self.live_bars.iter().any(|l| l.farm == super::pool::PRIMARY_HMDS && l.ticker_id == Some(tid));
        (!shared_router).then_some(tid)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_schedule_request(&mut self, req_id: ReqId, con_id: i64, sec_type: &str, exchange: &str, end_date_time: &str, duration: &str, use_rth: bool, hmds_conn: &mut dyn super::pool::FixSink, hb: &mut HeartbeatState) {
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
        let ts = chrono_free_timestamp();
        if hmds_conn.send_plain(&[
            (fix::TAG_MSG_TYPE, "W"),
            (fix::TAG_SENDING_TIME, &ts),
            (6118, &xml),
        ]) {
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent schedule request: req_id={} con_id={}", req_id, con_id);
        }
        self.pending_schedule.push((query_id, req_id));
    }

    /// Fail head timestamp queries past their deadline: 162 "Request
    /// Timed Out" after 5 s, as the reference (ibx#428). Bar and histogram
    /// queries have no deadline, as in the reference (ibx#485).
    pub(crate) fn sweep_head_timestamps(&mut self, shared: &SharedState) {
        if self.pending_head_ts.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut expired: Vec<ReqId> = Vec::new();
        self.pending_head_ts.retain(|(wid, req_id, deadline)| {
            if now >= *deadline {
                log::warn!("HMDS head timestamp timeout: req_id={} id={}", req_id, wid);
                expired.push(*req_id);
                false
            } else {
                true
            }
        });
        for req_id in expired {
            shared.reference.push_historical_error(req_id, 162, historical_service_error("Request Timed Out"));
        }
    }
}

/// The cancel of a 5-second router: `ticker:{id}` (ibx#429).
fn ticker_cancel_xml(ticker_id: u32) -> String {
    crate::control::historical::query_cancel_xml(&format!("ticker:{}", ticker_id))
}

/// Window id of the `<id>` of a reply (ibx#428).
fn reply_window_id(xml: &str) -> &str {
    crate::control::historical::extract_xml_tag(xml, "id")
        .map(crate::control::historical::window_id)
        .unwrap_or("")
}

/// The key of a stored head timestamp: contract, route, data (as the
/// API names it, upper case) and the RTH flag (`hmdscore.store.a(dy, flag,
/// ...)`; the flag is useRTH for the contracts ibx asks).
type HeadTsKey = (i64, String, String, bool);

fn head_ts_key(con_id: i64, sec_type: &str, exchange: &str, what_to_show: &str, use_rth: bool) -> HeadTsKey {
    let route = crate::engine::hot_loop::farm::routing_exchange(exchange, if sec_type.is_empty() { "STK" } else { sec_type });
    (con_id, route.to_string(), what_to_show.to_ascii_uppercase(), use_rth)
}

#[cfg(test)]
pub(crate) mod tests {
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
        hmds.pending_historical.push(("q7".to_string(), 21));

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
        hmds.pending_historical.push(("q8".to_string(), 22));

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
        hmds.pending_historical.push(("hist_1003".to_string(), 11));
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

    fn send_bid_ask(hmds: &mut HmdsState, shared: &SharedState, req_id: ReqId) -> (String, String) {
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.send_historical_request_ex(req_id, 416904, "IND", "CBOE", "", "3600 S", "1 min", "BID_ASK",
            true, false, "SPX", &mut conn, &mut hb, shared, false, 1, None, super::super::pool::PRIMARY_HMDS);
        let legs: Vec<&(String, ReqId)> =
            hmds.pending_historical.iter().filter(|(_, r)| *r == req_id).collect();
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
        // The times as the client gets them, in the zone of the reply
        // (ibx#431).
        assert_eq!(b, vec![
            // Both legs: open and low from Bid, close and high from Ask.
            ("20260227 15:30:00 US/Eastern", 266.466, 266.70, 266.30, 266.520),
            // Bid only.
            ("20260227 15:31:00 US/Eastern", 266.154, 266.154, 266.00, 266.154),
            // Ask only.
            ("20260227 15:32:00 US/Eastern", 266.100, 266.20, 266.100, 266.100),
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
        let cancels = hmds.cancel_bar_request(9, &shared);
        assert_eq!(cancels.len(), 2, "each waiting leg is cancelled by its id: {cancels:?}");
        assert!(cancels[0].1.contains("<CancelQuery><id>hist_1000</id></CancelQuery>"), "{:?}", cancels);
        assert!(hmds.pending_historical.is_empty());
        assert!(hmds.multi_leg.is_empty());
        let _ = (&mut hb, &mut conn);
    }


    #[test]
    fn bid_ask_keep_up_to_date_is_rejected() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.send_historical_request_ex(12, 416904, "IND", "CBOE", "", "3600 S", "5 secs", "BID_ASK",
            true, true, "SPX", &mut conn, &mut hb, &shared, false, 1, None, super::super::pool::PRIMARY_HMDS);
        assert!(hmds.pending_historical.is_empty() && hmds.live_bars.is_empty());
        // ibx#429: the reference's refusal.
        assert_eq!(shared.reference.drain_historical_errors(), vec![(12, 321,
            "Error validating request.-'bM' : cause - Source price not supported with live updates".to_string())]);
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
        hmds.send_head_timestamp_request(1, 265598, "STK", "SMART", "TRADES", true, 1, &mut conn, &mut hb, &shared);
        hmds.send_head_timestamp_request(2, 756733, "STK", "SMART", "TRADES", true, 1, &mut conn, &mut hb, &shared);
        let ids: Vec<&str> = hmds.pending_head_ts.iter().map(|(w, _, _)| w.as_str()).collect();
        assert_eq!(ids, vec!["TickHeadClient1", "TickHeadClient2"]);

        // The second request is answered first.
        hmds.process_hmds_message(&head_ts_reply("TickHeadClient2;;756733@BEST Last;;0;;true;;0;;U", "19930129-14:30:00"),
            &mut conn, &shared, &None, &mut hb);
        hmds.process_hmds_message(&head_ts_reply("TickHeadClient1;;265598@BEST Last;;0;;true;;0;;U", "19801212-14:30:00"),
            &mut conn, &shared, &None, &mut hb);
        let got: Vec<(ReqId, String)> = shared.reference.drain_head_timestamps().into_iter()
            .map(|(r, h)| (r, h.head_timestamp)).collect();
        assert_eq!(got, vec![(2, "19930129-14:30:00".to_string()), (1, "19801212-14:30:00".to_string())]);
        assert!(hmds.pending_head_ts.is_empty());
    }

    // ibx#486: a head timestamp the farm gave answers the next request of
    // the same contract, route, data and RTH flag, in its formatDate, with
    // no query; another RTH flag or data is asked.
    #[test]
    fn a_stored_head_timestamp_answers_the_next_request() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        assert!(!hmds.head_timestamp_from_cache(1, 265598, "STK", "SMART", "TRADES", true, 1, &shared, &None));
        hmds.send_head_timestamp_request(1, 265598, "STK", "SMART", "TRADES", true, 1, &mut conn, &mut hb, &shared);
        hmds.process_hmds_message(&head_ts_reply("TickHeadClient1;;265598@BEST Last;;0;;true;;0;;U", "19801212-14:30:00"),
            &mut conn, &shared, &None, &mut hb);
        shared.reference.drain_head_timestamps();
        assert!(hmds.head_timestamp_from_cache(2, 265598, "STK", "", "trades", true, 2, &shared, &None));
        let got: Vec<(ReqId, String)> = shared.reference.drain_head_timestamps().into_iter()
            .map(|(r, h)| (r, h.head_timestamp)).collect();
        assert_eq!(got, vec![(2, "345479400".to_string())]);
        assert!(!hmds.head_timestamp_from_cache(3, 265598, "STK", "SMART", "TRADES", false, 1, &shared, &None));
        assert!(!hmds.head_timestamp_from_cache(4, 265598, "STK", "SMART", "MIDPOINT", true, 1, &shared, &None));
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
        hmds.pending_historical.push(("hist_100".to_string(), 1));
        hmds.pending_historical.push(("hist_1000".to_string(), 2));
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
        hmds.send_historical_ticks_request(4, 265598, "AAPL", "STK", "SMART", "", "20260312-15:00:00", 100, "TRADES", true,
            false, None, super::super::pool::PRIMARY_HMDS, &mut conn, &mut hb, &shared);
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
            "<ResultSetTick><id>tk_70</id><eoq>{}</eoq><data>AllLast</data><Events><Tick><time>20260312-14:30:01</time>\
             <price>1.5</price><size>100</size></Tick></Events></ResultSetTick>", eoq));
        hmds.process_hmds_message(&frame(false), &mut conn, &shared, &None, &mut hb);
        assert_eq!(hmds.pending_ticks.len(), 2, "not done yet");
        hmds.process_hmds_message(&frame(true), &mut conn, &shared, &None, &mut hb);
        let got: Vec<(ReqId, bool)> = shared.reference.drain_historical_ticks().into_iter().map(|t| (t.0, t.3)).collect();
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
        hmds.send_fundamental_data_request(1, 265598, "ReportSnapshot", super::super::pool::PRIMARY_HMDS, &mut conn, &mut hb, &shared);
        hmds.send_fundamental_data_request(2, 272093, "ReportSnapshot", super::super::pool::PRIMARY_HMDS, &mut conn, &mut hb, &shared);
        assert_eq!(hmds.pending_fundamental[1].window_id, "Fundamentals2");
        let msg = fund_reply("Fundamentals2", "", b"<Snapshot/>");
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);
        let got = shared.reference.drain_fundamental_data();
        assert_eq!(got, [(2, "<Snapshot/>".to_string())]);
        assert_eq!(hmds.pending_fundamental.len(), 1);
        assert_eq!(hmds.pending_fundamental[0].req_id, 1);
    }

    /// A fundamentals reply: the id, a server error text when given, and
    /// the report gzip-compressed in the raw payload (none when empty).
    pub(crate) fn fund_reply(window_id: &str, error: &str, report: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let err = if error.is_empty() { String::new() } else { format!("<errorText>{error}</errorText>") };
        let mut msg = format!("8=O\x019=0\x0135=U\x016040=10012\x016118=<FundResponse><id>{window_id};; COMPANY_FUNDAMENTALS;;0;;true;;0;;U</id>{err}</FundResponse>\x01").into_bytes();
        if !report.is_empty() {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            gz.write_all(report).unwrap();
            let body = gz.finish().unwrap();
            msg.extend_from_slice(format!("95={}\x0196=", body.len()).as_bytes());
            msg.extend_from_slice(&body);
            msg.push(1);
        }
        msg
    }

    // #434: the report is the inflated raw payload (binary bytes kept
    // whole); a server error, an empty payload or a broken one is 430
    // with its cause; a report is kept and asked again it is answered
    // from memory; an unknown name is asked with no type; a waiting
    // request id is refused with 322.
    #[test]
    fn fundamentals_reply_errors_cache_and_types() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let primary = super::super::pool::PRIMARY_HMDS;
        // A report with bytes that are not text in every position of the
        // payload, and a separator byte inside the compressed data.
        let report = "<ReportFinancialSummary>caf\u{e9}\u{1}</ReportFinancialSummary>";
        hmds.send_fundamental_data_request(1, 265598, "ReportsFinSummary", primary, &mut conn, &mut hb, &shared);
        assert_eq!(hmds.pending_fundamental[0].window_id, "Morningstar1");
        hmds.send_fundamental_data_request(1, 265598, "ReportsFinSummary", primary, &mut conn, &mut hb, &shared);
        assert_eq!(shared.reference.drain_historical_errors(),
            [(1, 322, "Error processing request.-'bL' : cause - Duplicate ticker id".to_string())]);
        hmds.process_hmds_message(&fund_reply("Morningstar1", "", report.as_bytes()), &mut conn, &shared, &None, &mut hb);
        assert_eq!(shared.reference.drain_fundamental_data(), [(1, report.to_string())]);

        hmds.send_fundamental_data_request(2, 265598, "finsum", primary, &mut conn, &mut hb, &shared);
        assert!(hmds.pending_fundamental.is_empty(), "answered from memory");
        assert_eq!(shared.reference.drain_fundamental_data(), [(2, report.to_string())]);

        hmds.send_fundamental_data_request(3, 265598, "ReportsOwnership", primary, &mut conn, &mut hb, &shared);
        hmds.process_hmds_message(&fund_reply("Morningstar2", "Not allowed", b""), &mut conn, &shared, &None, &mut hb);
        hmds.send_fundamental_data_request(4, 265598, "BadName", primary, &mut conn, &mut hb, &shared);
        assert_eq!(hmds.pending_fundamental[0].report, "");
        hmds.process_hmds_message(&fund_reply("Fundamentals3", "", b""), &mut conn, &shared, &None, &mut hb);
        let not_available = crate::control::fundamental::FUNDAMENTALS_NOT_AVAILABLE;
        assert_eq!(shared.reference.drain_historical_errors(), [
            (3, 430, format!("{not_available}Not allowed")),
            (4, 430, format!("{not_available}Query failed")),
        ]);
        assert!(shared.reference.drain_fundamental_data().is_empty(), "errors are never data");

        // A cancel while waiting gives the query's cancel; after, nothing.
        hmds.send_fundamental_data_request(5, 272093, "ReportSnapshot", primary, &mut conn, &mut hb, &shared);
        let (farm, xml) = hmds.cancel_fundamental(5).unwrap();
        assert_eq!(farm, primary);
        assert!(xml.contains("<CancelQuery><id>Fundamentals4;; COMPANY_FUNDAMENTALS;;0;;true;;0;;U</id></CancelQuery>"), "{xml}");
        assert!(hmds.cancel_fundamental(5).is_none());
    }

    // A head timestamp times out after 5 s, as the reference (ibx#428);
    // bar and histogram queries have no deadline, as the reference
    // (ibx#485): they wait for their answer, with no error and no end.
    #[test]
    fn only_a_head_timestamp_times_out() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let past = Instant::now() - std::time::Duration::from_secs(1);
        hmds.pending_head_ts.push(("TickHeadClient1".to_string(), 1, past));
        hmds.pending_histogram.push(PendingHistogram {
            window_id: "histogramQuery0".into(), req_id: 2, sum: Default::default(),
        });
        send_bid_ask(&mut hmds, &shared, 10);
        hmds.pending_historical.push(("hist_1010".to_string(), 21));
        hmds.sweep_head_timestamps(&shared);
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors, [(1, 162, "Historical Market Data Service error message:Request Timed Out".to_string())]);
        assert!(hmds.pending_head_ts.is_empty());
        assert_eq!(hmds.pending_histogram.len(), 1);
        assert_eq!(hmds.pending_historical.len(), 3, "the two bid/ask legs and the bar query wait");
        assert_eq!(hmds.multi_leg.len(), 1);
        assert!(shared.reference.drain_historical_data().is_empty(), "no bar end");
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

    fn rt_sub(query_id: &str, req_id: ReqId, ticker_id: Option<u32>) -> RtBarSub {
        RtBarSub { query_id: query_id.to_string(), req_id, con_id: 265598, data: "Last", use_rth: true, ticker_id, min_tick: 0.01 }
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
        let got: Vec<(ReqId, u32)> = bars.iter().map(|(r, b)| (*r, b.timestamp)).collect();
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
        // The limit counts the routers, made by the answers.
        let ack = |id: &str, tid: u32| make_w_msg(&format!(
            "<ResultSetTickerId><id>{}</id><tickerId>{}</tickerId><minTick>0.01</minTick><eoq>false</eoq></ResultSetTickerId>", id, tid));
        let ids: Vec<String> = hmds.rtbar_subs.iter().map(|s| s.query_id.clone()).collect();
        hmds.process_hmds_message(&ack(&ids[0], 5), &mut conn, &shared, &None, &mut hb);
        hmds.process_hmds_message(&ack(&ids[1], 6), &mut conn, &shared, &None, &mut hb);
        hmds.send_realtime_bar_subscribe(4, 756733, "STK", "SMART", "SPY", "TRADES", true, &mut conn, &mut hb, &shared);
        // A request on an existing router does not add one.
        hmds.send_realtime_bar_subscribe(5, 265598, "STK", "SMART", "AAPL", "TRADES", true, &mut conn, &mut hb, &shared);
        assert_eq!(shared.reference.drain_historical_errors(), vec![
            (1, 321, "Error validating request.-'bS' : cause - What to show field is missing or incorrect.".to_string()),
            (2, 102, "Duplicate ticker id".to_string()),
            (4, 456, "Max number of real time requests has been reached".to_string()),
        ]);
        let reqs: Vec<ReqId> = hmds.rtbar_subs.iter().map(|s| s.req_id).collect();
        assert_eq!(reqs, vec![2, 3, 5]);
    }

    // ibx#454: a request for the contract, data and regular hours flag of
    // an answered request joins its router: no query, every bar to both;
    // another data, or another flag, is its own query. The router's cancel
    // goes with its last request; a request with no answer cancels
    // nothing. A second answer for a request on its router is 421 and
    // ends it.
    #[test]
    fn rtbar_requests_share_a_router() {
        use std::io::Read;
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let (client, mut server) = crate::protocol::connection::mem_pair();
        server.set_read_timeout(Some(std::time::Duration::from_millis(300))).unwrap();
        let mut conn = Some(Connection::new_mem(client));
        let mut sent = || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            while let Ok(n) = server.read(&mut chunk) {
                if n == 0 { break; }
                buf.extend_from_slice(&chunk[..n]);
            }
            String::from_utf8_lossy(&buf).matches("8=FIX").count()
        };
        let ack = |id: &str, tid: u32| make_w_msg(&format!(
            "<ResultSetTickerId><id>{}</id><tickerId>{}</tickerId><minTick>0.01</minTick><eoq>false</eoq></ResultSetTickerId>", id, tid));
        hmds.send_realtime_bar_subscribe(1, 265598, "STK", "SMART", "AAPL", "TRADES", true, &mut conn, &mut hb, &shared);
        let qid = hmds.rtbar_subs[0].query_id.clone();
        hmds.process_hmds_message(&ack(&qid, 5), &mut conn, &shared, &None, &mut hb);
        hmds.send_realtime_bar_subscribe(2, 265598, "STK", "SMART", "AAPL", "TRADES", true, &mut conn, &mut hb, &shared);
        hmds.send_realtime_bar_subscribe(3, 265598, "STK", "SMART", "AAPL", "TRADES", false, &mut conn, &mut hb, &shared);
        hmds.send_realtime_bar_subscribe(4, 265598, "STK", "SMART", "AAPL", "MIDPOINT", true, &mut conn, &mut hb, &shared);
        if let Some(c) = conn.as_mut() { let _ = c.flush_queued(); }
        assert_eq!(sent(), 3, "requests 1, 3 and 4 send a query");
        assert_eq!(hmds.rtbar_subs[1].ticker_id, Some(5));

        let payload = single_price_payload(15000, 100);
        hmds.process_hmds_message(&rtbar_frame(&[(5, 100, &payload)]), &mut conn, &shared, &None, &mut hb);
        let reqs: Vec<ReqId> = shared.market.drain_real_time_bars().iter().map(|(r, _)| *r).collect();
        assert_eq!(reqs, vec![1, 2]);

        assert_eq!(hmds.end_rtbar(1), None, "request 2 is left on the router");
        assert_eq!(hmds.end_rtbar(3), None, "no answer, nothing to cancel");
        assert_eq!(hmds.end_rtbar(2), Some(5));

        // 421: a second answer for request 4.
        let qid = hmds.rtbar_subs[0].query_id.clone();
        hmds.process_hmds_message(&ack(&qid, 7), &mut conn, &shared, &None, &mut hb);
        hmds.process_hmds_message(&ack(&qid, 7), &mut conn, &shared, &None, &mut hb);
        assert_eq!(shared.reference.drain_historical_errors(), vec![(4, 421, "Invalid Route".to_string())]);
        assert!(hmds.rtbar_subs.is_empty());
        if let Some(c) = conn.as_mut() { let _ = c.flush_queued(); }
        assert_eq!(sent(), 1, "its router's cancel");
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
            true, false, "SPY", &mut conn, &mut hb, &shared, false, 1, None, super::super::pool::PRIMARY_HMDS);

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
            true, false, "SPY", &mut conn, &mut hb, &shared, false, 1, None, super::super::pool::PRIMARY_HMDS);
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert_eq!(hmds.pending_historical.len(), 1);
        hmds.send_historical_request_ex(2, 756733, "STK", "SMART", "", "1 M", "1 day", "SCHEDULE",
            true, false, "SPY", &mut conn, &mut hb, &shared, false, 1, None, super::super::pool::PRIMARY_HMDS);
        assert_eq!(hmds.pending_historical.len(), 1, "a schedule is not a bar query");
        assert_eq!(hmds.pending_schedule.len(), 1);
        assert_eq!(hmds.pending_schedule[0].1, 2);
        hmds.send_historical_request_ex(3, 756733, "STK", "SMART", "", "1 M", "1 hour", "SCHEDULE",
            true, false, "SPY", &mut conn, &mut hb, &shared, false, 1, None, super::super::pool::PRIMARY_HMDS);
        assert_eq!(hmds.pending_schedule.len(), 1);
        assert_eq!(shared.reference.drain_historical_errors()[0].1, 321);
    }

    #[test]
    fn head_timestamp_with_unknown_what_to_show_is_refused_with_321() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.send_head_timestamp_request(4, 265598, "STK", "SMART", "TRADE", true, 1, &mut conn, &mut hb, &shared);
        assert!(hmds.pending_head_ts.is_empty());
        assert_eq!(shared.reference.drain_historical_errors(), vec![(
            4, 321, "Error validating request.-'bN' : cause - What to show value of TRADE rejected.".to_string(),
        )]);
    }

    #[test]
    fn query_error_for_unknown_query_id_drops_nothing_and_emits_no_error() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_historical.push(("hist_1003".to_string(), 11));

        let msg = make_query_error_msg("hist_9999", "Boom");
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

        assert_eq!(hmds.pending_historical.len(), 1, "unrelated entry must stay");
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert!(shared.reference.drain_historical_data().is_empty());
    }

    // ── Reference scenarios of 02/10/2026 (gateway 1040, AAPL pre-market) ──

    /// Inbound messages of the historical farms of a recorded scenario,
    /// decompressed, with their record number, in recorded order.
    fn fixture_messages(scenario: &str, conns: &[&str]) -> Vec<(u64, Vec<u8>)> {
        use base64::Engine as _;
        let path = format!("{}/tests/fixtures/gw1040/scenarios/{}", env!("CARGO_MANIFEST_DIR"), scenario);
        let text = std::fs::read_to_string(&path).unwrap();
        let mut out = Vec::new();
        for line in text.lines().skip(1) {
            let rec: serde_json::Value = serde_json::from_str(line).unwrap();
            if rec["leg"] != "fix_in" || !conns.iter().any(|c| rec["conn"] == *c) { continue; }
            let raw = base64::engine::general_purpose::STANDARD.decode(rec["raw_b64"].as_str().unwrap()).unwrap();
            let seq = rec["seq"].as_u64().unwrap();
            if raw.starts_with(b"8=FIXCOMP") {
                out.extend(crate::protocol::fixcomp::fixcomp_decompress(&raw).unwrap().into_iter().map(|m| (seq, m)));
            } else {
                // 5-second bars come as binary frames.
                out.push((seq, raw));
            }
        }
        out
    }

    /// A live request as `send_historical_request_ex` leaves it, with the
    /// window id of the recorded query.
    fn recorded_live(hmds: &mut HmdsState, req_id: ReqId, window_id: &str, label: &str,
                     bar_size: crate::control::historical::BarSize, trades: bool) {
        hmds.pending_historical.push((window_id.to_string(), req_id));
        hmds.keep_up_to_date_reqs.insert(req_id);
        hmds.bar_requests.push(BarRequestInfo {
            req_id, format_date: 1, intraday: bar_size.is_intraday(), zone: Some("US/Eastern".into()), start: 0, end: 0,
        });
        hmds.live_bars.push(LiveBars {
            req_id, window_id: window_id.to_string(), query_id: format!("{window_id};;AAPL@SMART {label};;1;;true;;0;;I"),
            farm: super::super::pool::PRIMARY_HMDS, ticker_id: None, min_tick: 0.01, bar_size, trades,
            series: Vec::new(), sessions: Vec::new(), history_done: false, reply_zone: String::new(),
        });
    }

    // ibx#429 (scenario b1_429_keep_up_to_date): four keepUpToDate
    // requests, three of them on one 5-second router. Each 5-second bar
    // gives each request the whole current bar of its size; the cancels
    // give 162, and the router cancel goes when its last request left.
    #[test]
    fn keep_up_to_date_updates_and_cancels_as_recorded() {
        use crate::control::historical::BarSize;
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        recorded_live(&mut hmds, 9530, "cf60", "Trades", BarSize::Hour1, true);
        recorded_live(&mut hmds, 9531, "cf64", "Trades", BarSize::Min1, true);
        recorded_live(&mut hmds, 9532, "cf68", "Midpoint", BarSize::Sec30, false);
        recorded_live(&mut hmds, 9533, "cf72", "Trades", BarSize::Hour2, true);
        // Up to the first cancel (record 4056).
        for (seq, msg) in fixture_messages("20261002/b1_429_keep_up_to_date.jsonl", &["ushmds"]) {
            if seq < 4056 {
                hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);
            }
        }
        assert!(shared.reference.drain_historical_errors().is_empty());
        let data = shared.reference.drain_historical_data();
        let bars = |req: ReqId| data.iter().filter(|(r, _)| *r == req).flat_map(|(_, d)| d.bars.clone()).collect::<Vec<_>>();
        assert_eq!(bars(9530).iter().map(|b| (b.time.as_str(), b.volume, b.count)).collect::<Vec<_>>(),
            [("20261002 04:00:00 US/Eastern", 49528, 522)]);
        let minute = bars(9531);
        assert_eq!((minute.len(), minute[0].time.as_str(), minute[60].time.as_str()),
            (61, "20261001 19:56:00 US/Eastern", "20261002 04:56:00 US/Eastern"));
        let mid = bars(9532);
        assert_eq!((mid.len(), mid[0].volume, mid[0].wap, mid[0].count), (61, -1, -1.0, -1));
        assert_eq!(bars(9533).len(), 9);

        let updates = shared.reference.drain_historical_updates();
        let of = |req: ReqId| updates.iter().filter(|(r, _)| *r == req)
            .map(|(_, b)| (b.time.clone(), b.open, b.high, b.low, b.close, b.volume, b.wap, b.count)).collect::<Vec<_>>();
        let hour = ("20261002 04:00:00 US/Eastern".to_string(), 331.05, 331.59, 330.55, 331.45, 49528, 331.285, 522);
        assert_eq!(of(9530), vec![hour.clone(); 9]);
        assert_eq!(of(9533), vec![hour; 9]);
        let m = |t: &str| (format!("20261002 {t} US/Eastern"), 331.45, 331.45, 331.45, 331.45, 0, 331.45, 0);
        assert_eq!(of(9531), [vec![m("04:56:00"); 5], vec![m("04:57:00"); 4]].concat());
        let h = |t: &str| (format!("20261002 {t} US/Eastern"), 331.42, 331.42, 331.42, 331.42, -1, -1.0, -1);
        assert_eq!(of(9532), [vec![h("04:56:30"); 5], vec![h("04:57:00"); 4]].concat());

        // The cancels, in the recorded order.
        assert!(hmds.cancel_bar_request(9530, &shared).is_empty(), "ticker 1 still used");
        assert!(hmds.cancel_bar_request(9531, &shared).is_empty());
        let c = hmds.cancel_bar_request(9532, &shared);
        assert_eq!(c.len(), 1);
        assert!(c[0].1.contains("<CancelQuery><id>ticker:2</id></CancelQuery>"), "{:?}", c);
        let c = hmds.cancel_bar_request(9533, &shared);
        assert!(c.len() == 1 && c[0].1.contains("<id>ticker:1</id>"), "{:?}", c);
        assert!(hmds.cancel_bar_request(99, &shared).is_empty());
        let cancelled = |r: ReqId| (r, 162, format!("Historical Market Data Service error message:API historical data query cancelled: {r}"));
        assert_eq!(shared.reference.drain_historical_errors(), vec![
            cancelled(9530), cancelled(9531), cancelled(9532), cancelled(9533),
            (99, 366, "No historical data query found for ticker id:99".to_string()),
        ]);
        assert!(hmds.live_bars.is_empty() && hmds.pending_historical.is_empty() && !hmds.has_live_listener());
        // No update after the cancel.
        for (seq, msg) in fixture_messages("20261002/b1_429_keep_up_to_date.jsonl", &["ushmds"]) {
            if seq >= 4056 {
                hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);
            }
        }
        assert!(shared.reference.drain_historical_updates().is_empty());
    }

    // ibx#432 (scenario b1_432_hist_ticks): every reply frame of a tick
    // query goes to its request with done at the last one; the ticks in
    // the order and number the server sent; the server's refusal is
    // 10187.
    #[test]
    fn historical_ticks_as_recorded() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let reqs = [("cf16", 9510, "AllLast"), ("cf20", 9511, "AllLast"), ("cf24", 9512, "BidAsk"), ("cf28", 9513, "BidAsk"),
            ("cf32", 9514, "MidPoint"), ("cf36", 9515, "AllLast"), ("cf40", 9516, "AllLast"), ("cf44", 9517, "AllLast"),
            ("cf48", 9518, "AllLast"), ("cf52", 9521, "AllLast"), ("cf56", 9522, "MidPoint")];
        for (w, r, d) in reqs {
            hmds.pending_ticks.push((w.to_string(), r, d.to_string()));
        }
        for (_, msg) in fixture_messages("20261002/b1_432_hist_ticks.jsonl", &["ushmds", "cashfarm"]) {
            hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);
        }
        assert!(hmds.pending_ticks.is_empty());
        assert_eq!(shared.reference.drain_historical_errors(), vec![(9518, 10187,
            "Failed to request historical ticks:2 out of startTime/endTime/timeLength parameters have to be specified".to_string())]);
        let got = shared.reference.drain_historical_ticks();
        let counts: Vec<(ReqId, usize, bool)> = got.iter().map(|(r, d, _, done)| (*r, match d {
            crate::types::HistoricalTickData::Last(v) => v.len(),
            crate::types::HistoricalTickData::BidAsk(v) => v.len(),
            crate::types::HistoricalTickData::Midpoint(v) => v.len(),
        }, *done)).collect();
        assert_eq!(counts, vec![(9510, 109, true), (9511, 1043, true), (9512, 108, true), (9513, 105, true), (9514, 105, true),
            (9515, 106, true), (9516, 109, true), (9517, 1261, true), (9521, 13, true), (9522, 25, true)]);
        let crate::types::HistoricalTickData::Last(t) = &got[5].1 else { panic!("trades") };
        assert_eq!((t[0].time, t[0].price, t[0].size, t[0].special_conditions.as_str(), t[0].tick_attrib_last.unreported),
            (1_790_881_192, 329.7621, 1.0, " 4 I", true));
        assert_eq!((t[1].price, t[1].size, t[1].exchange.as_str(), t[1].tick_attrib_last.unreported), (329.805, 140.0, "FINRA", false));
        let crate::types::HistoricalTickData::BidAsk(t) = &got[2].1 else { panic!("bid ask") };
        assert_eq!((t[1].time, t[1].price_bid, t[1].price_ask, t[1].size_bid, t[1].size_ask), (1_790_881_200, 329.81, 329.84, 40.0, 240.0));
        let crate::types::HistoricalTickData::Midpoint(t) = &got[9].1 else { panic!("currency pair midpoints") };
        assert_eq!((t[0].time, t[0].price, t[0].size), (1_790_881_199, 1.12347, 0.0));
    }

    // ibx#431 (scenario b1_431_hist_format): bar dates by formatDate and
    // the head timestamp by formatDate.
    #[test]
    fn bar_and_head_timestamp_dates_as_recorded() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let end = crate::control::historical::parse_server_time("20261002-08:57:40").unwrap();
        for (w, req, format_date, intraday) in [("cf76", 9540, 1, true), ("cf80", 9541, 2, true), ("cf84", 9542, 1, false), ("cf88", 9543, 2, false)] {
            hmds.pending_historical.push((w.to_string(), req));
            let duration = if intraday { "1 d" } else { "1 m" };
            hmds.bar_requests.push(BarRequestInfo { req_id: req, format_date, intraday, zone: Some("US/Eastern".into()),
                start: crate::control::historical::duration_start(end, duration, "Europe/Paris"), end });
        }
        hmds.pending_head_ts.push(("TickHeadClient1".to_string(), 9544, Instant::now() + HEAD_TIMESTAMP_TIMEOUT));
        hmds.head_ts_format.insert(9544, 2);
        for (_, msg) in fixture_messages("20261002/b1_431_hist_format.jsonl", &["ushmds"]) {
            hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);
        }
        let data = shared.reference.drain_historical_data();
        let dates = |req: ReqId| data.iter().filter(|(r, _)| *r == req)
            .flat_map(|(_, d)| d.bars.iter().map(|b| b.time.clone())).collect::<Vec<_>>();
        assert_eq!(dates(9540)[..2], ["20261001 09:30:00 US/Eastern".to_string(), "20261001 10:00:00 US/Eastern".to_string()]);
        assert_eq!(dates(9540).last().unwrap(), "20261001 15:00:00 US/Eastern");
        assert_eq!(dates(9541)[..2], ["1790861400".to_string(), "1790863200".to_string()]);
        for req in [9542, 9543] {
            let d = dates(req);
            assert_eq!((d.len(), d[0].as_str(), d[20].as_str()), (21, "20260902", "20261001"));
        }
        let ends: Vec<(ReqId, String, String)> = data.iter().filter(|(_, d)| d.is_complete)
            .map(|(r, d)| (*r, d.start.clone(), d.end.clone())).collect();
        assert_eq!(ends[0], (9540, "20261001 04:57:40 US/Eastern".into(), "20261002 04:57:40 US/Eastern".into()));
        assert_eq!(ends[2], (9542, "20260902 04:57:40 US/Eastern".into(), "20261002 04:57:40 US/Eastern".into()));
        assert!(hmds.bar_requests.is_empty(), "a one-shot request ends with its answer");
        let heads = shared.reference.drain_head_timestamps();
        assert_eq!(heads.iter().map(|(r, h)| (*r, h.head_timestamp.as_str())).collect::<Vec<_>>(), [(9544, "345479400")]);
        // The answered requests are not running any more: 366.
        assert!(hmds.cancel_bar_request(9540, &shared).is_empty());
        assert_eq!(shared.reference.drain_historical_errors()[0].1, 366);
    }

    #[test]
    fn a_waiting_one_shot_request_is_cancelled_by_its_query_id() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.send_historical_request_ex(5, 265598, "STK", "SMART", "", "1 D", "1 hour", "TRADES",
            true, false, "AAPL", &mut conn, &mut hb, &shared, false, 1, None, super::super::pool::PRIMARY_HMDS);
        let cancels = hmds.cancel_bar_request(5, &shared);
        assert_eq!(cancels.len(), 1);
        assert!(cancels[0].1.ends_with("<ListOfCancelQueries><CancelQuery><id>hist_1000</id></CancelQuery></ListOfCancelQueries>"), "{:?}", cancels);
        assert_eq!(shared.reference.drain_historical_errors(), vec![(5, 162,
            "Historical Market Data Service error message:API historical data query cancelled: 5".to_string())]);
    }

    #[test]
    fn historical_ticks_duplicate_and_zone_refusals() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let send = |hmds: &mut HmdsState, req: ReqId, start: &str, conn: &mut Option<Connection>, hb: &mut HeartbeatState| {
            hmds.send_historical_ticks_request(req, 265598, "AAPL", "STK", "SMART", start, "", 100, "TRADES", true, false,
                Some("US/Eastern"), super::super::pool::PRIMARY_HMDS, conn, hb, &shared);
        };
        send(&mut hmds, 1, "20261001 15:00:00 US/Eastern", &mut conn, &mut hb);
        send(&mut hmds, 1, "20261001 15:00:00 US/Eastern", &mut conn, &mut hb);
        send(&mut hmds, 2, "20261001 15:00:00 Asia/Tokyo", &mut conn, &mut hb);
        let errors = shared.reference.drain_historical_errors();
        assert_eq!((errors[0].0, errors[0].1, errors[0].2.as_str()), (1, 102, "Duplicate ticker id"));
        assert_eq!((errors[1].0, errors[1].1), (2, 10314));
        assert!(errors[1].2.starts_with("Start Date/Time:"));
        assert_eq!(hmds.pending_ticks.len(), 1);
        assert_eq!(hmds.pending_ticks[0].2, "AllLast");
    }

}
