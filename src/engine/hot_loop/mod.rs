pub mod farm;
pub mod ccp;
pub mod hmds;
pub mod order_builder;

use std::sync::Arc;
use std::time::Instant;
use std::io;

use crate::bridge::{Event, SharedState};
use crate::engine::context::Context;
use crate::config::chrono_free_timestamp;
use crate::gateway::{connect_farm, reconnect_ccp, ReconnectAuth, FARM_SLOT_HMDS, FARM_SLOT_TRADING};
use crate::protocol::connection::Connection;
use crate::protocol::fix;
use crate::types::{ControlCommand, Fill, InstrumentId, Price, Qty, TbtQuote, TbtTrade, PRICE_SCALE, QTY_SCALE};
use crossbeam_channel::{bounded, Receiver, Sender};

use farm::FarmState;
use ccp::CcpState;
use hmds::HmdsState;

/// Auth server heartbeat interval — single source in config (ibx#219
/// removed the duplicate definitions here).
const CCP_HEARTBEAT_SECS: u64 = crate::config::CCP_HEARTBEAT;
/// Farm heartbeat interval — single source in config.
const FARM_HEARTBEAT_SECS: u64 = crate::config::FARM_HEARTBEAT;
/// Liveness (ibx#219), aligned with the gateway's transport thresholds:
/// send a test request when nothing has been received for this long...
const LIVENESS_TEST_SECS: u64 = 15;
/// ...and declare the connection dead when nothing has been received for
/// this long. The old scheme declared death at ~21s — racing the server's
/// own ~35s reset and losing to transient stalls the server tolerates.
const LIVENESS_DEAD_SECS: u64 = 35;
/// Grace window after (re)connect before liveness is enforced (ibx#219):
/// early-connection jitter must not trigger a false disconnect during a
/// period the server itself treats as warm-up. Heartbeats are still sent.
const LIVENESS_WARMUP_SECS: u64 = 60;

/// The pinned-core hot loop. Pushes events to SharedState + optional event channel.
pub struct HotLoop {
    shared: Arc<SharedState>,
    event_tx: Option<Sender<Event>>,
    context: Context,
    /// Core ID to pin the hot loop thread to. None = no pinning.
    core_id: Option<usize>,
    /// Next scheduled CCP/farm reconnect attempt (jittered backoff, ibx#218).
    ccp_next_attempt_at: Option<Instant>,
    farm_next_attempt_at: Option<Instant>,
    /// Farm connection for market data (market data farm).
    pub farm_conn: Option<Connection>,
    /// Auth connection for order management.
    pub ccp_conn: Option<Connection>,
    /// Historical farm connection for historical data (optional).
    pub hmds_conn: Option<Connection>,
    /// SPSC channel receiver for control plane commands.
    control_rx: Option<Receiver<ControlCommand>>,
    /// Whether the hot loop should keep running.
    running: bool,
    /// Account ID for order submission.
    account_id: String,
    /// Heartbeat state.
    hb: HeartbeatState,
    /// Reusable buffer for control commands (avoids per-iteration allocation).
    cmd_buf: Vec<ControlCommand>,
    // ── Subsystems ──
    pub(crate) farm: FarmState,
    pub(crate) ccp: CcpState,
    pub(crate) hmds: HmdsState,
    // ── Auto-reconnect ──
    reconnect_auth: Option<ReconnectAuth>,
    pending_farm_reconnect: Option<Receiver<io::Result<Connection>>>,
    farm_reconnect_attempt: u32,
    pending_ccp_reconnect: Option<Receiver<io::Result<Connection>>>,
    ccp_reconnect_attempt: u32,
    /// HMDS reconnect state (ibx#187). Drives a background reconnect loop with
    /// exponential backoff when the historical-data farm is down — initial
    /// connect failed, or a future runtime disconnect detector trips it.
    pending_hmds_reconnect: Option<Receiver<io::Result<Connection>>>,
    hmds_reconnect_attempt: u32,
    /// Earliest instant the next HMDS reconnect attempt may spawn. `None` once
    /// retries are exhausted or HMDS is healthy.
    hmds_next_attempt_at: Option<Instant>,
}

/// Maximum HMDS reconnect attempts before giving up (ibx#187).
/// Total wait at cap: 3+6+12+24+48 = 93s before final attempt fires.
const HMDS_MAX_RECONNECT_ATTEMPTS: u32 = 6;

/// Tracks last send/recv times and pending test requests for heartbeat management.
pub struct HeartbeatState {
    pub last_ccp_sent: Instant,
    pub last_ccp_recv: Instant,
    pub last_farm_sent: Instant,
    pub last_farm_recv: Instant,
    pub last_hmds_sent: Instant,
    pub last_hmds_recv: Instant,
    /// Pending test request for auth: (test_req_id, sent_at).
    pub pending_ccp_test: Option<(String, Instant)>,
    /// When each connection (re)connected — liveness is not enforced during
    /// the warm-up window that follows (ibx#219).
    pub ccp_up_since: Instant,
    pub farm_up_since: Instant,
    pub hmds_up_since: Instant,
    /// Pending test request for farm: (test_req_id, sent_at).
    pub pending_farm_test: Option<(String, Instant)>,
    /// Pending test request for historical: (test_req_id, sent_at).
    pub pending_hmds_test: Option<(String, Instant)>,
    /// Counter for generating unique test request IDs.
    test_req_counter: u32,
}

impl HeartbeatState {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            last_ccp_sent: now,
            last_ccp_recv: now,
            last_farm_sent: now,
            last_farm_recv: now,
            last_hmds_sent: now,
            last_hmds_recv: now,
            pending_ccp_test: None,
            ccp_up_since: Instant::now(),
            farm_up_since: Instant::now(),
            hmds_up_since: Instant::now(),
            pending_farm_test: None,
            pending_hmds_test: None,
            test_req_counter: 0,
        }
    }

    fn next_test_id(&mut self) -> String {
        self.test_req_counter += 1;
        format!("T{}", self.test_req_counter)
    }
}

impl HotLoop {
    pub fn new(shared: Arc<SharedState>, event_tx: Option<Sender<Event>>, core_id: Option<usize>) -> Self {
        Self {
            shared,
            event_tx,
            context: Context::new(),
            core_id,
            farm_conn: None,
            ccp_conn: None,
            hmds_conn: None,
            control_rx: None,
            running: true,
            account_id: String::new(),
            hb: HeartbeatState::new(),
            cmd_buf: Vec::with_capacity(16),
            farm: FarmState::new(),
            ccp: CcpState::new(),
            hmds: HmdsState::new(),
            reconnect_auth: None,
            pending_farm_reconnect: None,
            ccp_next_attempt_at: None,
            farm_next_attempt_at: None,
            farm_reconnect_attempt: 0,
            pending_ccp_reconnect: None,
            ccp_reconnect_attempt: 0,
            pending_hmds_reconnect: None,
            hmds_reconnect_attempt: 0,
            hmds_next_attempt_at: None,
        }
    }

    /// Set the control channel receiver. The caller keeps the sender.
    pub fn set_control_rx(&mut self, rx: Receiver<ControlCommand>) {
        self.control_rx = Some(rx);
    }

    /// Set the account ID for order submission.
    pub fn set_account_id(&mut self, account_id: String) {
        self.account_id = account_id;
    }

    /// Access the context (for pre-start configuration like registering instruments).
    pub fn context_mut(&mut self) -> &mut Context {
        &mut self.context
    }

    /// Process pending control commands once. For testing.
    pub fn poll_once(&mut self) {
        self.poll_control_commands();
    }

    /// Whether the hot loop is still running. For testing.
    #[doc(hidden)]
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Build a HotLoop with connections and control channel, without requiring a Gateway.
    pub fn with_connections(
        shared: Arc<SharedState>,
        event_tx: Option<Sender<Event>>,
        account_id: String,
        farm_conn: Connection,
        ccp_conn: Connection,
        hmds_conn: Option<Connection>,
        core_id: Option<usize>,
    ) -> (Self, Sender<ControlCommand>) {
        let (tx, rx) = bounded(64);
        let mut hl = Self::new(shared, event_tx, core_id);
        hl.set_control_rx(rx);
        hl.set_account_id(account_id);
        hl.farm_conn = Some(farm_conn);
        hl.ccp_conn = Some(ccp_conn);
        hl.hmds_conn = hmds_conn;
        (hl, tx)
    }

    /// Run the hot loop under `catch_unwind`. On panic, log the payload and
    /// emit `Event::Disconnected` so consumers see the dead engine without
    /// having to wait for the next outbound call to fail. Use this from the
    /// engine-spawn site instead of `run()` directly (ibx#182).
    /// try_register + full-table rejection (ibx#233). On a full table the
    /// reply channel gets an Err — the caller's request fails loudly and the
    /// hot loop keeps running. Previously this was an assert! that killed
    /// the engine for the rest of the process.
    fn register_or_reject(
        &mut self,
        con_id: i64,
        symbol: String,
        sec_type: &str,
        exchange: &str,
        reply_tx: &Option<crossbeam_channel::Sender<Result<InstrumentId, String>>>,
    ) -> Option<InstrumentId> {
        match self.context.market.try_register(con_id) {
            Some(id) => {
                self.context.market.set_symbol(id, symbol);
                self.context.market.set_routing(id, sec_type, exchange);
                self.shared.market.set_instrument_count(self.context.market.count());
                if let Some(tx) = reply_tx { let _ = tx.send(Ok(id)); }
                Some(id)
            }
            None => {
                log::error!("Instrument table full: rejecting registration for con_id={}", con_id);
                if let Some(tx) = reply_tx {
                    let _ = tx.send(Err(format!(
                        "instrument table full: {} contracts are live concurrently; \
                         cancel unused market-data subscriptions to free slots",
                        crate::types::MAX_INSTRUMENTS
                    )));
                }
                None
            }
        }
    }

    /// Reclaim an instrument slot if nothing references it any more
    /// (ibx#233): no open orders, no tick-by-tick subscription, no news
    /// subscription. A reused id would repoint those references at the
    /// wrong contract, so referenced slots stay resident until released.
    fn try_reclaim_instrument(&mut self, instrument: InstrumentId) {
        if !self.context.open_orders_for(instrument).is_empty() {
            return;
        }
        if self.hmds.tbt_subscriptions.iter().any(|(id, _, _)| *id == instrument) {
            return;
        }
        if self.ccp.news_subscriptions.iter().any(|(id, _)| *id == instrument) {
            return;
        }
        if self.context.market.unregister(instrument).is_some() {
            // Zero the shared-side quote so a reused slot cannot serve the
            // previous contract's prices before its first tick.
            self.shared.market.push_quote(instrument, &crate::types::Quote::default());
            log::info!("Reclaimed instrument slot {}", instrument);
        }
    }

    pub fn run_with_panic_recovery(mut self) {
        let event_tx = self.event_tx.clone();
        let shared = self.shared.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.run();
        }));
        if let Err(payload) = result {
            let msg: &str = payload
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| payload.downcast_ref::<&'static str>().copied())
                .unwrap_or("<non-string panic payload>");
            log::error!("Engine hot loop panicked, emitting Disconnected: {}", msg);
            shared.set_connection_lost();
            emit(&event_tx, Event::Disconnected);
        }
    }

    /// Run the hot loop. Blocks until Shutdown command received.
    pub fn run(&mut self) {
        if let Some(core) = self.core_id {
            Self::pin_to_core(core);
        }

        self.running = true;

        while self.running {
            self.context.loop_iterations += 1;

            // 1. Busy-poll market data farm socket (non-blocking recv)
            let farm_was_ok = !self.farm.disconnected;
            self.farm.poll_market_data(
                &mut self.farm_conn, &mut self.context, &self.shared,
                &self.event_tx, &mut self.hb,
            );
            let _ = farm_was_ok; // reconnects are scheduled below (ibx#218)

            // 1b. Busy-poll historical socket for tick-by-tick data
            self.hmds.poll(
                &mut self.hmds_conn, &self.shared,
                &self.event_tx, &mut self.hb,
            );

            // 1c. Hand off any scanner results with cache-miss con_ids to CCP for
            //     contract-detail fan-out (ibx#156). Mirrors what the gateway does
            //     internally for binary-API scanner clients — see ib-agent#142.
            for (req_id, result) in self.hmds.cold_scanner_results.drain(..).collect::<Vec<_>>() {
                self.ccp.start_scanner_enrichment(
                    req_id, result, &mut self.ccp_conn, &self.shared, &mut self.hb,
                );
            }

            // 2. Drain pending orders → build → sign → send to auth
            //    Skip if CCP is disconnected — orders stay in buffer for retry after reconnect.
            order_builder::drain_and_send_orders(
                &mut self.ccp_conn, &mut self.context, &self.account_id, &mut self.hb,
                self.ccp.disconnected, &self.shared,
            );

            // 3. Busy-poll auth socket for execution reports
            let ccp_was_ok = !self.ccp.disconnected;
            self.ccp.poll_executions(
                &mut self.ccp_conn, &mut self.context, &self.shared,
                &self.event_tx, &mut self.hb, &self.account_id,
            );
            self.ccp.sweep_pending_schedule_pairs(&self.shared, &self.event_tx);
            self.ccp.sweep_scanner_enrichments(&self.shared);
            self.ccp.sweep_contract_details(&self.shared, &self.event_tx);
            self.hmds.sweep_pending_historical(&self.shared);
            let _ = ccp_was_ok; // reconnects are scheduled below (ibx#218)

            // 4. Check control_plane_rx (SPSC) for commands
            self.poll_control_commands();

            // 5. Heartbeat check (auth 10s, farm 30s)
            self.check_heartbeats();

            // 5b. Poll pending reconnects and schedule the next attempts
            //     (jittered backoff instead of immediate re-dials, ibx#218)
            self.poll_farm_reconnect();
            self.poll_ccp_reconnect();
            self.poll_hmds_reconnect();
            self.maybe_spawn_farm_reconnect();
            self.maybe_spawn_ccp_reconnect();
            self.maybe_spawn_hmds_reconnect();

            // 6. Wake any waiting consumers (e.g. Python event loop)
            self.shared.notify();
        }
    }

    fn emit_hmds_unavailable(&self, req_id: u32, from_historical: bool) {
        push_hmds_unavailable(&self.shared, req_id, from_historical);
    }

    fn poll_control_commands(&mut self) {
        let rx = match self.control_rx.as_ref() {
            Some(rx) => rx,
            None => return,
        };

        self.cmd_buf.clear();
        self.cmd_buf.extend(rx.try_iter());

        // try_iter() stops on both Empty and Disconnected — do one extra
        // try_recv() to distinguish.  If a straggler command arrived between
        // try_iter() finishing and this call, push it into the batch.
        let sender_dropped = match rx.try_recv() {
            Ok(cmd)  => { self.cmd_buf.push(cmd); false }
            Err(crossbeam_channel::TryRecvError::Empty)        => false,
            Err(crossbeam_channel::TryRecvError::Disconnected) => true,
        };

        // Drain the buffer so we can mutably borrow self in the loop body.
        let cmds: Vec<ControlCommand> = self.cmd_buf.drain(..).collect();
        for cmd in cmds {
            match cmd {
                ControlCommand::Subscribe { con_id, symbol, exchange, sec_type, last_trade_date, strike, right, multiplier, mode_9887, reply_tx } => {
                    if let Some(id) = self.register_or_reject(con_id, symbol.clone(), &sec_type, &exchange, &reply_tx) {
                        self.farm.send_mktdata_subscribe(
                            con_id, &symbol, &exchange, &sec_type,
                            &last_trade_date, strike, &right, &multiplier,
                            id, mode_9887,
                            &mut self.farm_conn,
                            &mut self.hb,
                        );
                    }
                }
                ControlCommand::Unsubscribe { instrument } => {
                    self.farm.send_mktdata_unsubscribe(
                        instrument,
                        &mut self.farm_conn,
                        &mut self.hb,
                    );
                    self.try_reclaim_instrument(instrument);
                }
                ControlCommand::SubscribeTbt { con_id, symbol, tbt_type, reply_tx } => {
                    if let Some(id) = self.register_or_reject(con_id, symbol, "", "", &reply_tx) {
                        self.hmds.send_tbt_subscribe(con_id, id, tbt_type, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::UnsubscribeTbt { instrument } => {
                    self.hmds.send_tbt_unsubscribe(instrument, &mut self.hmds_conn, &mut self.hb);
                    self.try_reclaim_instrument(instrument);
                }
                ControlCommand::SubscribeNews { con_id, symbol, providers, reply_tx } => {
                    if let Some(id) = self.register_or_reject(con_id, symbol, "", "", &reply_tx) {
                        // Allocate req_id from farm's counter (shared ID space)
                        let req_id = self.farm.next_md_req_id;
                        self.farm.next_md_req_id += 1;
                        self.ccp.send_news_subscribe(con_id, id, &providers, req_id, &mut self.ccp_conn, &mut self.hb);
                    }
                }
                ControlCommand::UnsubscribeNews { instrument } => {
                    self.ccp.send_news_unsubscribe(instrument, &mut self.ccp_conn, &mut self.hb);
                    self.try_reclaim_instrument(instrument);
                }
                ControlCommand::UpdateParam { key, value } => {
                    let _ = (key, value);
                }
                ControlCommand::Ping => {
                    // On-demand RTT sample (ibx#158). Reuses the liveness
                    // test-request machinery; a pending liveness test is
                    // already a measurement in flight, so don't stomp it.
                    if self.hb.pending_ccp_test.is_none() {
                        if let Some(conn) = self.ccp_conn.as_mut() {
                            let ts = chrono_free_timestamp();
                            let test_id = self.hb.next_test_id();
                            let _ = conn.send_fix(&[
                                (fix::TAG_MSG_TYPE, fix::MSG_TEST_REQUEST),
                                (fix::TAG_SENDING_TIME, &ts),
                                (fix::TAG_TEST_REQ_ID, &test_id),
                            ]);
                            self.hb.pending_ccp_test = Some((test_id, Instant::now()));
                            self.hb.last_ccp_sent = Instant::now();
                        }
                    }
                }
                ControlCommand::Order(req) => {
                    self.context.pending_orders.push(req);
                }
                ControlCommand::RegisterInstrument { con_id, symbol, sec_type, exchange, reply_tx } => {
                    self.register_or_reject(con_id, symbol, &sec_type, &exchange, &reply_tx);
                }
                ControlCommand::FetchHistorical { req_id, con_id, symbol, end_date_time, duration, bar_size, what_to_show, use_rth, keep_up_to_date } => {
                    // keepUpToDate sends via CCP but bars/end arrive on HMDS — both
                    // paths require an authed HMDS socket to deliver a completion.
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, true);
                    } else if keep_up_to_date {
                        if self.hmds.send_historical_request_via_ccp(req_id, con_id, &end_date_time, &duration, &bar_size, &what_to_show, use_rth, &symbol, &mut self.ccp_conn, &mut self.hb, &self.ccp.ccp_sign_key, &self.ccp.ccp_sign_iv, &self.shared) {
                            self.hmds.keep_up_to_date_reqs.insert(req_id);
                        }
                    } else {
                        self.hmds.send_historical_request_ex(req_id, con_id, &end_date_time, &duration, &bar_size, &what_to_show, use_rth, false, &symbol, &mut self.hmds_conn, &mut self.hb, &self.shared);
                    }
                }
                ControlCommand::CancelHistorical { req_id } => {
                    self.hmds.keep_up_to_date_reqs.remove(&req_id);
                    if let Some(pos) = self.hmds.pending_historical.iter().position(|(_, rid, _)| *rid == req_id) {
                        let (query_id, _, _) = self.hmds.pending_historical.remove(pos);
                        self.hmds.send_historical_cancel(&query_id, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::FetchHeadTimestamp { req_id, con_id, what_to_show, use_rth } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_head_timestamp_request(req_id, con_id, &what_to_show, use_rth, &mut self.hmds_conn, &mut self.hb, &self.shared);
                    }
                }
                ControlCommand::FetchContractDetails { req_id, con_id, symbol, sec_type, exchange, currency, filters } => {
                    if con_id > 0 {
                        self.ccp.send_secdef_request(req_id, con_id, &mut self.ccp_conn, &mut self.hb);
                    } else {
                        self.ccp.send_secdef_request_by_symbol(req_id, &symbol, &sec_type, &exchange, &currency, &filters, &mut self.ccp_conn, &mut self.hb);
                    }
                }
                ControlCommand::CancelHeadTimestamp { req_id } => {
                    if let Some(pos) = self.hmds.pending_head_ts.iter().position(|(_, rid)| *rid == req_id) {
                        self.hmds.pending_head_ts.remove(pos);
                    }
                }
                ControlCommand::FetchMatchingSymbols { req_id, pattern } => {
                    self.ccp.send_matching_symbols_request(req_id, &pattern, &mut self.ccp_conn, &mut self.hb);
                }
                ControlCommand::FetchMktDepthExchanges => {
                    self.ccp.send_mkt_depth_exchanges_request(&mut self.ccp_conn, &mut self.hb, &self.shared);
                }
                ControlCommand::FetchScannerParams => {
                    self.hmds.send_scanner_params_request(&mut self.hmds_conn, &mut self.hb);
                }
                ControlCommand::SubscribeScanner { req_id, instrument, location_code, scan_code, max_items } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_scanner_subscribe(req_id, &instrument, &location_code, &scan_code, max_items, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::CancelScanner { req_id } => {
                    if let Some(pos) = self.hmds.pending_scanner.iter().position(|(_, rid)| *rid == req_id) {
                        let (scan_id, _) = self.hmds.pending_scanner.remove(pos);
                        self.hmds.send_scanner_cancel(&scan_id, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::FetchHistoricalNews { req_id, con_id, provider_codes, start_time, end_time, max_results } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_historical_news_request(req_id, con_id, &provider_codes, &start_time, &end_time, max_results, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::FetchNewsArticle { req_id, provider_code, article_id } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_news_article_request(req_id, &provider_code, &article_id, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::FetchFundamentalData { req_id, con_id, report_type } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_fundamental_data_request(req_id, con_id, &report_type, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::CancelFundamentalData { req_id } => {
                    if let Some(pos) = self.hmds.pending_fundamental.iter().position(|(_, rid)| *rid == req_id) {
                        self.hmds.pending_fundamental.remove(pos);
                    }
                }
                ControlCommand::FetchHistogramData { req_id, con_id, use_rth, period } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_histogram_request(req_id, con_id, use_rth, &period, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::CancelHistogramData { req_id } => {
                    if let Some(pos) = self.hmds.pending_histogram.iter().position(|(_, rid)| *rid == req_id) {
                        self.hmds.pending_histogram.remove(pos);
                    }
                }
                ControlCommand::FetchHistoricalTicks { req_id, con_id, start_date_time, end_date_time, number_of_ticks, what_to_show, use_rth } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_historical_ticks_request(req_id, con_id, &start_date_time, &end_date_time, number_of_ticks, &what_to_show, use_rth, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::SubscribeRealTimeBar { req_id, con_id, symbol, what_to_show, use_rth } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_realtime_bar_subscribe(req_id, con_id, &symbol, &what_to_show, use_rth, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::CancelRealTimeBar { req_id } => {
                    if let Some(pos) = self.hmds.rtbar_subs.iter().position(|(_, rid, _, _)| *rid == req_id) {
                        let (query_id, _, ticker_id, _) = self.hmds.rtbar_subs.remove(pos);
                        let cancel_id = ticker_id.map(|t| t.to_string()).unwrap_or(query_id);
                        self.hmds.send_historical_cancel(&cancel_id, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::FetchHistoricalSchedule { req_id, con_id, end_date_time, duration, use_rth } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_schedule_request(req_id, con_id, &end_date_time, &duration, use_rth, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::SubscribeDepth { req_id, con_id, exchange, sec_type, num_rows, is_smart_depth } => {
                    self.farm.send_depth_subscribe(
                        req_id, con_id, &exchange, &sec_type, num_rows, is_smart_depth,
                        &mut self.farm_conn,
                        &mut self.hb,
                    );
                }
                ControlCommand::UnsubscribeDepth { req_id } => {
                    self.farm.send_depth_unsubscribe(
                        req_id,
                        &mut self.farm_conn,
                        &mut self.hb,
                    );
                    // Purge any already-buffered depth updates so callers never see stale data
                    self.shared.market.purge_depth_updates(req_id);
                }
                ControlCommand::SubscribePnl { req_id, account } => {
                    self.ccp.send_pnl_subscribe(req_id, &account, &mut self.ccp_conn, &mut self.hb);
                }
                ControlCommand::CancelPnl { req_id } => {
                    let _ = req_id; // Server auto-cancels on disconnect; no explicit cancel message needed
                }
                ControlCommand::FetchNewsProviders { .. }
                | ControlCommand::FetchSmartComponents { .. }
                | ControlCommand::FetchSoftDollarTiers { .. }
                | ControlCommand::FetchUserInfo { .. } => {
                    // Gateway-local data — handled synchronously in Python EClient.
                    // These variants exist for future CCP round-trip support.
                }
                ControlCommand::Shutdown => {
                    // Unsubscribe all active market data before stopping
                    let instruments: Vec<InstrumentId> = self.farm.instrument_md_reqs
                        .iter().map(|(id, _)| *id).collect();
                    for instrument in instruments {
                        self.farm.send_mktdata_unsubscribe(
                            instrument,
                            &mut self.farm_conn,
                            &mut self.hb,
                        );
                    }
                    // Unsubscribe all TBT subscriptions before stopping
                    let tbt_instruments: Vec<InstrumentId> = self.hmds.tbt_subscriptions
                        .iter().map(|(id, _, _)| *id).collect();
                    for instrument in tbt_instruments {
                        self.hmds.send_tbt_unsubscribe(instrument, &mut self.hmds_conn, &mut self.hb);
                    }
                    // Unsubscribe all news subscriptions before stopping
                    let news_instruments: Vec<InstrumentId> = self.ccp.news_subscriptions
                        .iter().map(|(id, _)| *id).collect();
                    for instrument in news_instruments {
                        self.ccp.send_news_unsubscribe(instrument, &mut self.ccp_conn, &mut self.hb);
                    }
                    self.running = false;
                    self.shared.set_connection_lost();
                    emit(&self.event_tx, Event::Disconnected);
                }
            }
        }

        // All senders dropped — treat as implicit shutdown.
        if sender_dropped && self.running {
            log::warn!("Control channel disconnected — shutting down hot loop");
            self.running = false;
            self.shared.set_connection_lost();
            emit(&self.event_tx, Event::Disconnected);
        }
    }

    fn check_heartbeats(&mut self) {
        let now = Instant::now();
        let ts = chrono_free_timestamp();

        // --- Auth heartbeat (skip if already disconnected) ---
        if !self.ccp.disconnected {
        if let Some(conn) = self.ccp_conn.as_mut() {
            let since_sent = now.duration_since(self.hb.last_ccp_sent).as_secs();
            let since_recv = now.duration_since(self.hb.last_ccp_recv).as_secs();

            if since_sent >= CCP_HEARTBEAT_SECS {
                let _ = conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                    (fix::TAG_SENDING_TIME, &ts),
                ]);
                self.hb.last_ccp_sent = now;
            }

            let warmed_up = now.duration_since(self.hb.ccp_up_since).as_secs() >= LIVENESS_WARMUP_SECS;
            if warmed_up && since_recv > LIVENESS_TEST_SECS {
                if since_recv > LIVENESS_DEAD_SECS {
                    log::error!("CCP liveness timeout ({}s silent) — connection lost", since_recv);
                    self.ccp.handle_disconnect(&mut self.context, &self.event_tx);
                } else if self.hb.pending_ccp_test.is_none() {
                    let test_id = self.hb.next_test_id();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_TEST_REQUEST),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    self.hb.pending_ccp_test = Some((test_id, now));
                    self.hb.last_ccp_sent = now;
                }
            }
        }
        }

        // --- Farm heartbeat (skip if already disconnected) ---
        if !self.farm.disconnected {
        if let Some(conn) = self.farm_conn.as_mut() {
            let since_sent = now.duration_since(self.hb.last_farm_sent).as_secs();
            let since_recv = now.duration_since(self.hb.last_farm_recv).as_secs();

            if since_sent >= FARM_HEARTBEAT_SECS {
                let _ = conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                    (fix::TAG_SENDING_TIME, &ts),
                ]);
                self.hb.last_farm_sent = now;
            }

            let warmed_up = now.duration_since(self.hb.farm_up_since).as_secs() >= LIVENESS_WARMUP_SECS;
            if warmed_up && since_recv > LIVENESS_TEST_SECS {
                if since_recv > LIVENESS_DEAD_SECS {
                    log::error!("Farm liveness timeout ({}s silent) — connection lost", since_recv);
                    self.farm.handle_disconnect(&mut self.context, &self.event_tx);
                } else if self.hb.pending_farm_test.is_none() {
                    let test_id = self.hb.next_test_id();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_TEST_REQUEST),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    self.hb.pending_farm_test = Some((test_id, now));
                    self.hb.last_farm_sent = now;
                }
            }
        }
        }

        // --- Historical heartbeat (skip if disconnected or no historical activity) ---
        if !self.hmds.disconnected && self.hmds_conn.is_some() {
        if let Some(conn) = self.hmds_conn.as_mut() {
            let since_sent = now.duration_since(self.hb.last_hmds_sent).as_secs();
            let since_recv = now.duration_since(self.hb.last_hmds_recv).as_secs();

            if since_sent >= FARM_HEARTBEAT_SECS {
                let _ = conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                    (fix::TAG_SENDING_TIME, &ts),
                ]);
                self.hb.last_hmds_sent = now;
            }

            let warmed_up = now.duration_since(self.hb.hmds_up_since).as_secs() >= LIVENESS_WARMUP_SECS;
            if warmed_up && since_recv > LIVENESS_TEST_SECS {
                if since_recv > LIVENESS_DEAD_SECS {
                    log::error!("HMDS liveness timeout ({}s silent) — connection lost", since_recv);
                    self.hmds.disconnected = true;
                } else if self.hb.pending_hmds_test.is_none() {
                    let test_id = self.hb.next_test_id();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_TEST_REQUEST),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    self.hb.pending_hmds_test = Some((test_id, now));
                    self.hb.last_hmds_sent = now;
                }
            }
        }
        }
    }

    fn pin_to_core(core: usize) {
        let core_ids = core_affinity::get_core_ids().unwrap_or_default();
        if let Some(id) = core_ids.get(core) {
            core_affinity::set_for_current(*id);
        }
    }

    /// Whether the farm connection has been lost.
    pub fn is_farm_disconnected(&self) -> bool {
        self.farm.disconnected
    }

    /// Whether the auth connection has been lost.
    pub fn is_ccp_disconnected(&self) -> bool {
        self.ccp.disconnected
    }

    /// Replace the farm connection (after reconnection) and re-subscribe to all instruments.
    pub fn reconnect_farm(&mut self, conn: Connection) {
        self.farm.reconnect(
            conn,
            &mut self.farm_conn,
            &mut self.context, &mut self.hb,
        );
    }

    /// Replace the auth connection (after reconnection) and reconcile order state.
    pub fn reconnect_ccp(&mut self, conn: Connection) {
        self.ccp.reconnect(conn, &mut self.ccp_conn, &mut self.hb, &self.account_id);
    }

    /// Set cached auth credentials for farm auto-reconnect.
    pub fn set_reconnect_auth(&mut self, auth: ReconnectAuth) {
        self.reconnect_auth = Some(auth);
    }

    /// Update caller-specific fields on the reconnect auth (host, username, password, paper).
    pub fn update_reconnect_auth(
        &mut self,
        host: String,
        username: String,
        password: zeroize::Zeroizing<String>,
        paper: bool,
    ) {
        if let Some(auth) = self.reconnect_auth.as_mut() {
            auth.host = host;
            auth.username = username;
            auth.password = password;
            auth.paper = paper;
        }
    }

    /// Schedule-then-spawn farm reconnects on the jittered backoff ladder
    /// (ibx#218). Called every loop iteration; no-op while connected or an
    /// attempt is in flight.
    fn maybe_spawn_farm_reconnect(&mut self) {
        if !self.farm.disconnected || self.pending_farm_reconnect.is_some() {
            return;
        }
        match self.farm_next_attempt_at {
            None => {
                let delay = reconnect_backoff(self.farm_reconnect_attempt);
                log::info!("Farm reconnect attempt {} scheduled in {:?} (ibx#218)",
                    self.farm_reconnect_attempt + 1, delay);
                self.farm_next_attempt_at = Some(Instant::now() + delay);
            }
            Some(due) if Instant::now() >= due => {
                self.farm_next_attempt_at = None;
                self.spawn_farm_reconnect();
                if self.pending_farm_reconnect.is_none() {
                    // Could not spawn (no cached credentials): re-check in a
                    // minute instead of warn-spamming every iteration.
                    self.farm_next_attempt_at =
                        Some(Instant::now() + std::time::Duration::from_secs(60));
                }
            }
            _ => {}
        }
    }

    /// See `maybe_spawn_farm_reconnect`.
    fn maybe_spawn_ccp_reconnect(&mut self) {
        if !self.ccp.disconnected || self.pending_ccp_reconnect.is_some() {
            return;
        }
        match self.ccp_next_attempt_at {
            None => {
                let delay = reconnect_backoff(self.ccp_reconnect_attempt);
                log::info!("CCP reconnect attempt {} scheduled in {:?} (ibx#218)",
                    self.ccp_reconnect_attempt + 1, delay);
                self.ccp_next_attempt_at = Some(Instant::now() + delay);
            }
            Some(due) if Instant::now() >= due => {
                self.ccp_next_attempt_at = None;
                self.spawn_ccp_reconnect();
                if self.pending_ccp_reconnect.is_none() {
                    self.ccp_next_attempt_at =
                        Some(Instant::now() + std::time::Duration::from_secs(60));
                }
            }
            _ => {}
        }
    }

    /// Spawn a background thread to reconnect the farm using cached credentials.
    fn spawn_farm_reconnect(&mut self) {
        if self.pending_farm_reconnect.is_some() { return; } // already in progress
        let auth = match self.reconnect_auth.clone() {
            Some(a) if !a.host.is_empty() => a,
            _ => {
                log::warn!("Farm auto-reconnect skipped: no credentials (host empty or auth missing)");
                return;
            }
        };
        self.farm_reconnect_attempt += 1;
        let attempt = self.farm_reconnect_attempt;
        log::info!("Farm auto-reconnect attempt {} starting (host={}, user={})", attempt, auth.host, auth.username);

        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name(format!("farm-reconnect-{}", attempt))
            .spawn(move || {
                let result = connect_farm(
                    &auth.host, "usfarm",
                    &auth.username, &auth.password, auth.paper,
                    &auth.server_session_id, &auth.session_key,
                    &auth.hw_info, &auth.encoded, FARM_SLOT_TRADING,
                );
                let _ = tx.send(result);
            })
            .ok();
        self.pending_farm_reconnect = Some(rx);
    }

    /// Poll for a completed farm reconnect. Non-blocking.
    fn poll_farm_reconnect(&mut self) {
        let rx = match self.pending_farm_reconnect.as_ref() {
            Some(rx) => rx,
            None => return,
        };
        match rx.try_recv() {
            Ok(Ok(conn)) => {
                log::info!("Farm auto-reconnect succeeded (attempt {})", self.farm_reconnect_attempt);
                self.reconnect_farm(conn);
                self.farm_reconnect_attempt = 0;
                self.farm_next_attempt_at = None;
                self.hb.farm_up_since = Instant::now();
                self.pending_farm_reconnect = None;
            }
            Ok(Err(e)) => {
                log::error!("Farm auto-reconnect failed (attempt {}): {}", self.farm_reconnect_attempt, e);
                self.pending_farm_reconnect = None;
                // Notify once after three straight failures; retries continue
                // on the backoff ladder — the old 3-attempt hard cap gave up
                // sooner than the gateway would (ibx#218).
                if self.farm_reconnect_attempt == 3 {
                    log::error!("Farm auto-reconnect failed 3 times — notifying (retries continue)");
                    self.shared.set_connection_lost();
                    emit(&self.event_tx, Event::Disconnected);
                }
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                log::error!("Farm reconnect thread dropped without result");
                self.pending_farm_reconnect = None;
            }
        }
    }

    /// Spawn a background thread to reconnect CCP using cached credentials.
    fn spawn_ccp_reconnect(&mut self) {
        if self.pending_ccp_reconnect.is_some() { return; }
        let auth = match self.reconnect_auth.clone() {
            Some(a) if !a.host.is_empty() => a,
            _ => {
                log::warn!("CCP auto-reconnect skipped: no credentials");
                return;
            }
        };
        self.ccp_reconnect_attempt += 1;
        let attempt = self.ccp_reconnect_attempt;
        log::info!("CCP auto-reconnect attempt {} starting (host={})", attempt, auth.host);

        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name(format!("ccp-reconnect-{}", attempt))
            .spawn(move || {
                let _ = tx.send(reconnect_ccp(&auth));
            })
            .ok();
        self.pending_ccp_reconnect = Some(rx);
    }

    /// Poll for a completed CCP reconnect. Non-blocking.
    fn poll_ccp_reconnect(&mut self) {
        let rx = match self.pending_ccp_reconnect.as_ref() {
            Some(rx) => rx,
            None => return,
        };
        match rx.try_recv() {
            Ok(Ok(conn)) => {
                log::info!("CCP auto-reconnect succeeded (attempt {})", self.ccp_reconnect_attempt);
                self.reconnect_ccp(conn);
                self.ccp_reconnect_attempt = 0;
                self.ccp_next_attempt_at = None;
                self.hb.ccp_up_since = Instant::now();
                self.pending_ccp_reconnect = None;
            }
            Ok(Err(e)) => {
                log::error!("CCP auto-reconnect failed (attempt {}): {}", self.ccp_reconnect_attempt, e);
                self.pending_ccp_reconnect = None;
                // See the farm path: notify once, keep retrying (ibx#218).
                if self.ccp_reconnect_attempt == 3 {
                    log::error!("CCP auto-reconnect failed 3 times — notifying (retries continue)");
                    self.shared.set_connection_lost();
                    emit(&self.event_tx, Event::Disconnected);
                }
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                log::error!("CCP reconnect thread dropped without result");
                self.pending_ccp_reconnect = None;
            }
        }
    }

    /// If HMDS is down and a backoff window has elapsed, spawn the next attempt.
    /// Auto-schedules the first attempt when the engine starts with no HMDS
    /// connection — covers the ibx#187 case where initial soft-token returned
    /// FAILED and the gateway dropped the socket.
    fn maybe_spawn_hmds_reconnect(&mut self) {
        if self.hmds_conn.is_some() { return; }
        if self.pending_hmds_reconnect.is_some() { return; }
        let auth = match self.reconnect_auth.as_ref() {
            Some(a) if !a.host.is_empty() && !a.hmds_host.is_empty() => a,
            _ => return,
        };
        if self.hmds_reconnect_attempt >= HMDS_MAX_RECONNECT_ATTEMPTS {
            return;
        }
        // Schedule the first attempt if not already scheduled.
        if self.hmds_next_attempt_at.is_none() {
            self.hmds_next_attempt_at = Some(Instant::now() + hmds_reconnect_backoff(self.hmds_reconnect_attempt + 1));
            return;
        }
        let due = self.hmds_next_attempt_at.unwrap();
        if Instant::now() < due { return; }
        let auth = auth.clone();
        self.hmds_reconnect_attempt += 1;
        let attempt = self.hmds_reconnect_attempt;
        log::info!(
            "HMDS reconnect attempt {} starting (host={}/{})",
            attempt, auth.hmds_host, auth.hmds_farm,
        );
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name(format!("hmds-reconnect-{}", attempt))
            .spawn(move || {
                let result = connect_farm(
                    &auth.hmds_host, &auth.hmds_farm,
                    &auth.username, &auth.password, auth.paper,
                    &auth.server_session_id, &auth.session_key,
                    &auth.hw_info, &auth.encoded, FARM_SLOT_HMDS,
                );
                let _ = tx.send(result);
            })
            .ok();
        self.pending_hmds_reconnect = Some(rx);
    }

    /// Poll for a completed HMDS reconnect. Non-blocking.
    fn poll_hmds_reconnect(&mut self) {
        let rx = match self.pending_hmds_reconnect.as_ref() {
            Some(rx) => rx,
            None => return,
        };
        match rx.try_recv() {
            Ok(Ok(conn)) => {
                log::info!("HMDS reconnect succeeded (attempt {})", self.hmds_reconnect_attempt);
                self.hmds_conn = Some(conn);
                self.hmds.disconnected = false;
                self.hb.last_hmds_recv = Instant::now();
                self.hb.last_hmds_sent = Instant::now();
                self.hmds_reconnect_attempt = 0;
                self.hmds_next_attempt_at = None;
                self.pending_hmds_reconnect = None;
            }
            Ok(Err(e)) => {
                log::warn!(
                    "HMDS reconnect failed (attempt {}/{}): {}",
                    self.hmds_reconnect_attempt, HMDS_MAX_RECONNECT_ATTEMPTS, e,
                );
                self.pending_hmds_reconnect = None;
                if self.hmds_reconnect_attempt >= HMDS_MAX_RECONNECT_ATTEMPTS {
                    log::error!(
                        "HMDS reconnect exhausted {} attempts — historical data unavailable for this session",
                        HMDS_MAX_RECONNECT_ATTEMPTS,
                    );
                    self.hmds_next_attempt_at = None;
                } else {
                    self.hmds_next_attempt_at = Some(Instant::now() + hmds_reconnect_backoff(self.hmds_reconnect_attempt + 1));
                }
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                log::error!("HMDS reconnect thread dropped without result");
                self.pending_hmds_reconnect = None;
            }
        }
    }

    /// Access heartbeat state for testing.
    pub fn heartbeat_state(&self) -> &HeartbeatState {
        &self.hb
    }

    /// Test-only: force farm into disconnected state.
    pub fn force_farm_disconnect(&mut self) {
        self.farm.handle_disconnect_for_test();
    }

    /// Test-only: trigger farm reconnect spawn.
    pub fn spawn_farm_reconnect_for_test(&mut self) {
        self.spawn_farm_reconnect();
    }

    /// Test-only: poll pending farm reconnect.
    pub fn poll_farm_reconnect_for_test(&mut self) {
        self.poll_farm_reconnect();
    }

    /// Mutably access heartbeat state for testing (e.g., setting timestamps).
    pub fn heartbeat_state_mut(&mut self) -> &mut HeartbeatState {
        &mut self.hb
    }

    /// Inject a raw farm message for testing. Processes it through the full decode pipeline.
    pub fn inject_farm_message(&mut self, msg: &[u8]) {
        self.farm.process_farm_message(msg, &mut self.farm_conn, &mut self.context, &self.shared, &self.event_tx, &mut self.hb);
    }

    /// Inject a raw auth message for testing. Processes execution reports, etc.
    pub fn inject_ccp_message(&mut self, msg: &[u8]) {
        self.ccp.process_ccp_message(msg, &mut self.ccp_conn, &mut self.context, &self.shared, &self.event_tx, &mut self.hb, &self.account_id);
    }

    /// Inject a raw HMDS message for testing. Processes historical data, news, etc.
    pub fn inject_hmds_message(&mut self, msg: &[u8]) {
        self.hmds.process_hmds_message(msg, &mut self.hmds_conn, &self.shared, &self.event_tx, &mut self.hb);
    }

    /// Inject a TBT trade for testing. Pushes to SharedState and emits event.
    pub fn inject_tbt_trade(&mut self, trade: &TbtTrade) {
        self.shared.market.push_tbt_trade(trade.clone());
        emit(&self.event_tx, Event::TbtTrade(trade.clone()));
    }

    /// Inject a TBT quote for testing. Pushes to SharedState.
    pub fn inject_tbt_quote(&mut self, quote: &TbtQuote) {
        self.shared.market.push_tbt_quote(quote.clone());
    }

    /// Inject a simulated tick for testing.
    pub fn inject_tick(&mut self, instrument: InstrumentId) {
        self.shared.market.push_quote(instrument, self.context.quote(instrument));
        emit(&self.event_tx, Event::Tick(instrument));
    }

    /// Simulate a fill for testing. Updates position and notifies.
    pub fn inject_fill(&mut self, fill: &Fill) {
        let delta = match fill.side {
            crate::types::Side::Buy => fill.qty,
            crate::types::Side::Sell | crate::types::Side::ShortSell => -fill.qty,
        };
        self.context.update_position(fill.instrument, delta);
        self.shared.orders.push_fill(*fill);
        self.shared.portfolio.set_position(fill.instrument, self.context.position(fill.instrument));
        emit(&self.event_tx, Event::Fill(*fill));
    }
}

// ── Helper functions used by subsystems ──

/// Stack-allocated string (up to 24 bytes). Zero heap allocations.
pub(crate) struct StackStr {
    buf: [u8; 24],
    len: u8,
}

impl StackStr {
    #[inline]
    fn new() -> Self {
        Self { buf: [0; 24], len: 0 }
    }

    #[inline]
    fn push(&mut self, b: u8) {
        self.buf[self.len as usize] = b;
        self.len += 1;
    }

    /// Write an i64 in decimal. Returns number of bytes written.
    fn write_i64(&mut self, val: i64) {
        if val < 0 {
            self.push(b'-');
            self.write_u64((-val) as u64);
        } else {
            self.write_u64(val as u64);
        }
    }

    fn write_u64(&mut self, val: u64) {
        if val == 0 {
            self.push(b'0');
            return;
        }
        // Write digits in reverse, then reverse them in-place.
        let start = self.len as usize;
        let mut v = val;
        while v > 0 {
            self.push(b'0' + (v % 10) as u8);
            v /= 10;
        }
        self.buf[start..self.len as usize].reverse();
    }
}

impl std::ops::Deref for StackStr {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        // SAFETY: We only write ASCII digits, '.', '-', and ':'
        unsafe { std::str::from_utf8_unchecked(&self.buf[..self.len as usize]) }
    }
}

impl std::fmt::Display for StackStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}

impl std::fmt::Debug for StackStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}

/// Format an unsigned integer to a stack string. Zero alloc.
#[inline]
pub(crate) fn format_uint(val: u64) -> StackStr {
    let mut s = StackStr::new();
    s.write_u64(val);
    s
}

/// Emit an event to the channel (if connected). Non-blocking — drops event if full.
#[inline]
pub(crate) fn emit(event_tx: &Option<Sender<Event>>, event: Event) {
    if let Some(tx) = event_tx {
        let _ = tx.try_send(event);
    }
}

/// Clone a payload for the event channel, but only when one is attached.
///
/// Use this wherever the payload is a deep copy (bar batches, contract
/// definitions): the value goes to `SharedState` by move and the clone is paid
/// for only when someone is listening. With no channel — the default for the
/// Rust client — nothing is copied at all (ibx#242).
///
/// Clone first, push second, emit last, so the event never becomes visible
/// before the same data is readable from `SharedState`.
#[inline]
pub(crate) fn clone_for_event<T: Clone>(event_tx: &Option<Sender<Event>>, value: &T) -> Option<T> {
    event_tx.as_ref().map(|_| value.clone())
}

/// Backoff schedule for HMDS reconnect attempts (ibx#187, ib-agent#153).
/// `min(64, 3 * 2^(attempt-1))` seconds — approximates the captured cadence
/// of 3.2 / 11.4 / 18.5 / 42.7 / 63.7 s the official client uses.
#[inline]
/// Jittered reconnect backoff for CCP/farm (ibx#218), mirroring the
/// gateway's ladder: delay = min(2s floor + ladder + jitter, 82s). The
/// ladder climbs 0/5/15/30/50/60s per consecutive failure and the jitter
/// range grows 5s -> 20s (+5s per failure). Immediate rapid-fire re-dials
/// risk server-side rate limiting and eviction. (HMDS keeps its own
/// capture-matched doubling schedule below.)
pub(crate) fn reconnect_backoff(failures: u32) -> std::time::Duration {
    const LADDER_MS: [u64; 6] = [0, 5_000, 15_000, 30_000, 50_000, 60_000];
    let ladder = LADDER_MS[(failures as usize).min(LADDER_MS.len() - 1)];
    let jitter_max = (5_000 + 5_000 * failures as u64).min(20_000);
    let jitter = rand::random::<u64>() % jitter_max;
    std::time::Duration::from_millis((2_000 + ladder + jitter).min(82_000))
}

pub(crate) fn hmds_reconnect_backoff(attempt: u32) -> std::time::Duration {
    let n = attempt.saturating_sub(1).min(31);
    let secs = (3u64.saturating_mul(1u64 << n)).min(64);
    std::time::Duration::from_secs(secs)
}

/// Surface an "HMDS unavailable" error for `req_id` when the historical-data
/// socket isn't connected. Mirrors the QueryError surface (ibx#186): code 162
/// via `push_historical_error` for the consumer's `error()` callback, plus —
/// for historical-bar requests only — a terminal empty-bars response so
/// `historical_data_end` fires. Without this, requests issued while HMDS is
/// down hang silently (ibx#187).
pub(crate) fn push_hmds_unavailable(shared: &SharedState, req_id: u32, from_historical: bool) {
    push_hmds_error(
        shared, req_id,
        "Historical data service connection is not available".to_string(),
        from_historical,
    );
}

/// Surface an HMDS-side request failure: error 162 plus, for bar requests,
/// the terminal completion sentinel so a blocked wait unblocks.
pub(crate) fn push_hmds_error(shared: &SharedState, req_id: u32, message: String, from_historical: bool) {
    const HMDS_ERROR_CODE: i32 = 162;
    shared.reference.push_historical_error(
        req_id,
        HMDS_ERROR_CODE,
        message,
    );
    if from_historical {
        shared.reference.push_historical_data(
            req_id,
            crate::control::historical::HistoricalResponse {
                query_id: String::new(),
                timezone: String::new(),
                is_complete: true,
                bars: Vec::new(),
            },
        );
    }
}

/// Format a fixed-point Price as a decimal string for FIX tags. Zero alloc.
pub(crate) fn format_price(price: Price) -> StackStr {
    let whole = price / PRICE_SCALE;
    let frac = (price % PRICE_SCALE).unsigned_abs();
    let mut s = StackStr::new();
    s.write_i64(whole);
    if frac != 0 {
        s.push(b'.');
        // Write 8-digit zero-padded fraction, then trim trailing zeros.
        let frac_start = s.len as usize;
        let digits = [
            b'0' + (frac / 10_000_000 % 10) as u8,
            b'0' + (frac / 1_000_000 % 10) as u8,
            b'0' + (frac / 100_000 % 10) as u8,
            b'0' + (frac / 10_000 % 10) as u8,
            b'0' + (frac / 1_000 % 10) as u8,
            b'0' + (frac / 100 % 10) as u8,
            b'0' + (frac / 10 % 10) as u8,
            b'0' + (frac % 10) as u8,
        ];
        // Find last non-zero digit.
        let mut end = 8;
        while end > 0 && digits[end - 1] == b'0' { end -= 1; }
        for i in 0..end {
            s.buf[frac_start + i] = digits[i];
        }
        s.len = (frac_start + end) as u8;
    }
    s
}

/// Parse a FIX tag value as a Price (fixed-point). Returns 0 if absent,
/// unparseable, or non-finite. Rust's f64 parser accepts "nan"/"inf", but on
/// the wire those are not-available sentinels, not values: the gateway's own
/// field parser maps nan/unparseable to unset (ibx#214). Without the finite
/// filter, "nan" saturated to 0 and "inf" to i64::MAX.
pub(crate) fn parse_price_tag(val: Option<&String>) -> Price {
    val.and_then(|s| s.parse::<f64>().ok())
        .filter(|f| f.is_finite())
        .map(|f| (f * PRICE_SCALE as f64) as Price)
        .unwrap_or(0)
}

/// Decode a wire TIF byte to the API TIF string. Exact inverse of
/// `api::types::Order::tif_byte` (DTC also encodes to '6' and decodes as GTD).
/// The old inline map decoded '7' (never emitted) as OPG and dropped
/// OPG ('2') and AUC ('8') to "" (ibx#220).
pub(crate) fn decode_tif(tif: u8) -> &'static str {
    match tif {
        b'0' => "DAY", b'1' => "GTC", b'2' => "OPG", b'3' => "IOC",
        b'4' => "FOK", b'6' => "GTD", b'8' => "AUC", _ => "",
    }
}

/// Format a fixed-point Qty (QTY_SCALE = 10^4) to a decimal string. Zero alloc.
pub(crate) fn format_qty(qty: Qty) -> StackStr {
    let whole = qty / QTY_SCALE;
    let frac = (qty % QTY_SCALE).unsigned_abs();
    let mut s = StackStr::new();
    s.write_i64(whole);
    if frac != 0 {
        s.push(b'.');
        let frac_start = s.len as usize;
        let digits = [
            b'0' + (frac / 1_000 % 10) as u8,
            b'0' + (frac / 100 % 10) as u8,
            b'0' + (frac / 10 % 10) as u8,
            b'0' + (frac % 10) as u8,
        ];
        let mut end = 4;
        while end > 0 && digits[end - 1] == b'0' { end -= 1; }
        for i in 0..end {
            s.buf[frac_start + i] = digits[i];
        }
        s.len = (frac_start + end) as u8;
    }
    s
}

/// Fast extraction of FIX tag 35 (MsgType) value via byte scan.
pub(crate) fn fast_extract_msg_type(msg: &[u8]) -> Option<&[u8]> {
    let limit = msg.len().min(48);
    let mut i = 0;
    while i + 3 < limit {
        if msg[i] == b'3' && msg[i + 1] == b'5' && msg[i + 2] == b'=' {
            if i == 0 || msg[i - 1] == 0x01 {
                let val_start = i + 3;
                let mut j = val_start;
                while j < msg.len() && msg[j] != 0x01 {
                    j += 1;
                }
                if j > val_start {
                    return Some(&msg[val_start..j]);
                }
            }
        }
        i += 1;
    }
    None
}

pub(crate) fn find_body_after_tag<'a>(msg: &'a [u8], tag_marker: &[u8]) -> Option<&'a [u8]> {
    msg.windows(tag_marker.len())
        .position(|w| w == tag_marker)
        .map(|pos| &msg[pos + tag_marker.len()..])
}

/// Extract the raw bytes of a binary FIX tag value using a length tag.
pub(crate) fn extract_raw_tag(msg: &[u8], tag: u32) -> Option<Vec<u8>> {
    let len_tag = tag - 1;
    if let Some(len_val) = extract_text_tag(msg, len_tag) {
        if let Ok(data_len) = len_val.parse::<usize>() {
            let needle = format!("{}=", tag);
            let needle_bytes = needle.as_bytes();
            if let Some(idx) = msg.windows(needle_bytes.len()).position(|w| w == needle_bytes) {
                let val_start = idx + needle_bytes.len();
                let val_end = (val_start + data_len).min(msg.len());
                return Some(msg[val_start..val_end].to_vec());
            }
        }
    }
    let needle = format!("{}=", tag);
    let needle_bytes = needle.as_bytes();
    let mut pos = 0;
    while pos < msg.len() {
        let remaining = &msg[pos..];
        if let Some(idx) = remaining.windows(needle_bytes.len()).position(|w| w == needle_bytes) {
            let abs_idx = pos + idx;
            if abs_idx == 0 || msg[abs_idx - 1] == 0x01 {
                let val_start = abs_idx + needle_bytes.len();
                let val_end = msg[val_start..].iter().position(|&b| b == 0x01)
                    .map(|p| val_start + p)
                    .unwrap_or(msg.len());
                return Some(msg[val_start..val_end].to_vec());
            }
            pos = abs_idx + 1;
        } else {
            break;
        }
    }
    None
}

/// Extract a text FIX tag value (SOH-delimited) from raw message bytes.
fn extract_text_tag(msg: &[u8], tag: u32) -> Option<String> {
    let needle = format!("{}=", tag);
    let needle_bytes = needle.as_bytes();
    let mut pos = 0;
    while pos < msg.len() {
        let remaining = &msg[pos..];
        if let Some(idx) = remaining.windows(needle_bytes.len()).position(|w| w == needle_bytes) {
            let abs_idx = pos + idx;
            if abs_idx == 0 || msg[abs_idx - 1] == 0x01 {
                let val_start = abs_idx + needle_bytes.len();
                let val_end = msg[val_start..].iter().position(|&b| b == 0x01)
                    .map(|p| val_start + p)
                    .unwrap_or(msg.len());
                return Some(String::from_utf8_lossy(&msg[val_start..val_end]).into_owned());
            }
            pos = abs_idx + 1;
        } else {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::bridge::{Event, SharedState};
    use crate::types::*;
    use std::time::Duration;

    // ibx#214: f64::from_str accepts "nan"/"inf", so a not-available sentinel
    // of "nan" collapsed to price 0 (and "inf" saturated to i64::MAX) instead
    // of being treated as unset.
    #[test]
    fn parse_price_tag_rejects_non_finite_sentinels() {
        let s = |v: &str| v.to_string();
        assert_eq!(parse_price_tag(Some(&s("nan"))), 0);
        assert_eq!(parse_price_tag(Some(&s("NaN"))), 0);
        assert_eq!(parse_price_tag(Some(&s("inf"))), 0);
        assert_eq!(parse_price_tag(Some(&s("-inf"))), 0);
        assert_eq!(parse_price_tag(Some(&s("n/a"))), 0);
        assert_eq!(parse_price_tag(None), 0);
        // Genuine numbers still parse, including a true zero.
        assert_eq!(parse_price_tag(Some(&s("0"))), 0);
        assert_eq!(parse_price_tag(Some(&s("434.71"))), (434.71 * PRICE_SCALE as f64) as Price);
        assert_eq!(parse_price_tag(Some(&s("-1.5"))), (-1.5 * PRICE_SCALE as f64) as Price);
    }

    #[test]
    fn inject_tick_emits_events() {
        let shared = Arc::new(SharedState::new());
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut engine = HotLoop::new(shared.clone(), Some(event_tx), None);
        engine.context_mut().market.register(265598);

        engine.inject_tick(0);
        engine.inject_tick(0);

        let events: Vec<Event> = event_rx.try_iter().collect();
        let tick_count = events.iter().filter(|e| matches!(e, Event::Tick(_))).count();
        assert_eq!(tick_count, 2);
    }

    #[test]
    fn inject_tick_multiple_instruments() {
        let shared = Arc::new(SharedState::new());
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut engine = HotLoop::new(shared.clone(), Some(event_tx), None);
        engine.context_mut().market.register(265598); // 0: AAPL
        engine.context_mut().market.register(272093); // 1: MSFT

        engine.inject_tick(0);
        engine.inject_tick(1);

        let events: Vec<Event> = event_rx.try_iter().collect();
        let tick_events: Vec<_> = events.iter().filter_map(|e| match e {
            Event::Tick(id) => Some(*id),
            _ => None,
        }).collect();
        assert_eq!(tick_events, vec![0, 1]);
    }

    #[test]
    fn inject_fill_updates_position() {
        let shared = Arc::new(SharedState::new());
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut engine = HotLoop::new(shared.clone(), Some(event_tx), None);
        engine.context_mut().market.register(265598);

        let fill = Fill {
            instrument: 0,
            order_id: 1001,
            side: Side::Buy,
            price: 150_00000000,
            qty: 100,
            remaining: 0,
            commission: 1_00000000,
            timestamp_ns: 0,
        };
        engine.inject_fill(&fill);
        assert_eq!(engine.context_mut().position(0), 100);
    }

    #[test]
    fn heartbeat_state_accessible() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared, None, None);
        let hb = engine.heartbeat_state_mut();
        hb.last_farm_sent = Instant::now() - Duration::from_secs(60);
        assert!(engine.heartbeat_state().last_farm_sent.elapsed().as_secs() >= 59);
    }

    #[test]
    fn shutdown_sets_running_false() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared, None, None);
        engine.set_control_rx(rx);
        engine.running = true;
        tx.send(ControlCommand::Shutdown).unwrap();
        engine.poll_once();
        assert!(!engine.is_running());
    }

    #[test]
    fn channel_disconnect_stops_loop() {
        let shared = Arc::new(SharedState::new());
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared, Some(event_tx), None);
        engine.set_control_rx(rx);
        engine.running = true;

        // Drop sender — simulates EClient being dropped without disconnect().
        drop(tx);

        engine.poll_once();
        assert!(!engine.is_running(), "hot loop should stop when control channel disconnects");

        // Should emit Disconnected event.
        let events: Vec<Event> = event_rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, Event::Disconnected)));
    }

    #[test]
    fn shutdown_sets_connection_lost_flag_without_event_channel() {
        // ibx#242: the flag path must work with no event channel attached,
        // which is the default for the Rust client.
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_control_rx(rx);
        engine.running = true;
        tx.send(ControlCommand::Shutdown).unwrap();
        engine.poll_once();

        assert!(shared.take_connection_lost(), "shutdown must signal connection lost");
        assert!(!shared.take_connection_lost(), "flag must clear after being read");
    }

    #[test]
    fn channel_disconnect_sets_connection_lost_flag() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_control_rx(rx);
        engine.running = true;
        drop(tx);
        engine.poll_once();

        assert!(shared.take_connection_lost());
    }

    #[test]
    fn clone_for_event_skips_the_copy_when_no_channel() {
        // ibx#242: with no listener the deep copy must not happen at all.
        let payload = vec![1u8, 2, 3];
        assert!(clone_for_event(&None, &payload).is_none());

        let (tx, _rx) = crossbeam_channel::bounded::<Event>(1);
        assert_eq!(clone_for_event(&Some(tx), &payload), Some(payload));
    }

    #[test]
    fn run_exits_on_shutdown() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared, None, None);
        engine.set_control_rx(rx);

        // Send Shutdown before run() starts — run() should drain it and exit.
        tx.send(ControlCommand::Shutdown).unwrap();

        // run() should return (not hang).
        engine.run();
        assert!(!engine.is_running());
    }

    #[test]
    fn run_exits_on_channel_disconnect() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared, None, None);
        engine.set_control_rx(rx);

        // Drop sender — run() should detect disconnect and exit.
        drop(tx);

        engine.run();
        assert!(!engine.is_running());
    }

    #[test]
    fn push_hmds_unavailable_historical_emits_error_and_terminal_sentinel() {
        let shared = SharedState::new();
        push_hmds_unavailable(&shared, 7, true);

        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, 7);
        assert_eq!(errors[0].1, 162);
        assert!(errors[0].2.contains("not available"));

        let hist = shared.reference.drain_historical_data();
        assert_eq!(hist.len(), 1, "terminal sentinel required so historical_data_end fires");
        assert_eq!(hist[0].0, 7);
        assert!(hist[0].1.is_complete);
        assert!(hist[0].1.bars.is_empty());
    }

    #[test]
    // ibx#218: the CCP/farm ladder — floor 2s, ladder 0/5/15/30/50/60s,
    // jitter range growing 5s -> 20s, ceiling 82s.
    #[test]
    fn reconnect_backoff_ladder_bounds() {
        use std::time::Duration;
        let expect = |failures: u32, lo: u64, hi: u64| {
            for _ in 0..50 {
                let d = reconnect_backoff(failures);
                assert!(d >= Duration::from_millis(lo) && d < Duration::from_millis(hi),
                    "failures={} got {:?}, expected [{}ms, {}ms)", failures, d, lo, hi);
            }
        };
        expect(0, 2_000, 7_000);    // 2s + 0 + jitter(0..5s)
        expect(1, 7_000, 17_000);   // 2s + 5s + jitter(0..10s)
        expect(2, 17_000, 32_000);  // 2s + 15s + jitter(0..15s)
        expect(3, 32_000, 52_000);  // 2s + 30s + jitter(0..20s)
        expect(5, 62_000, 82_001);  // 2s + 60s + jitter(0..20s), capped 82s
        expect(50, 62_000, 82_001); // ladder index capped
    }

    // ibx#219: the liveness ladder must be ordered and inside the server's
    // own thresholds (test at 15s, dead at 35s, warm-up 60s).
    #[test]
    fn liveness_thresholds_ordered() {
        assert!(CCP_HEARTBEAT_SECS < LIVENESS_TEST_SECS);
        assert!(LIVENESS_TEST_SECS < LIVENESS_DEAD_SECS);
        assert_eq!(LIVENESS_TEST_SECS, 15);
        assert_eq!(LIVENESS_DEAD_SECS, 35);
        assert_eq!(LIVENESS_WARMUP_SECS, 60);
        // The duplicate interval constants are gone — these now alias config.
        assert_eq!(CCP_HEARTBEAT_SECS, crate::config::CCP_HEARTBEAT);
        assert_eq!(FARM_HEARTBEAT_SECS, crate::config::FARM_HEARTBEAT);
    }

    #[test]
    fn hmds_reconnect_backoff_matches_captured_cadence() {
        use std::time::Duration;
        // Captured cadence (ib-agent#153): 3.2 / 11.4 / 18.5 / 42.7 / 63.7 s.
        // Our schedule: 3 / 6 / 12 / 24 / 48 / 64 s — captures the doubling
        // shape and caps at the 64 s ceiling.
        assert_eq!(hmds_reconnect_backoff(1), Duration::from_secs(3));
        assert_eq!(hmds_reconnect_backoff(2), Duration::from_secs(6));
        assert_eq!(hmds_reconnect_backoff(3), Duration::from_secs(12));
        assert_eq!(hmds_reconnect_backoff(4), Duration::from_secs(24));
        assert_eq!(hmds_reconnect_backoff(5), Duration::from_secs(48));
        assert_eq!(hmds_reconnect_backoff(6), Duration::from_secs(64));
        // Cap holds for any further attempts.
        assert_eq!(hmds_reconnect_backoff(7), Duration::from_secs(64));
        assert_eq!(hmds_reconnect_backoff(100), Duration::from_secs(64));
        // Saturating math survives degenerate inputs.
        assert_eq!(hmds_reconnect_backoff(0), Duration::from_secs(3));
        assert_eq!(hmds_reconnect_backoff(u32::MAX), Duration::from_secs(64));
    }

    #[test]
    fn push_hmds_unavailable_non_historical_emits_error_without_sentinel() {
        let shared = SharedState::new();
        push_hmds_unavailable(&shared, 42, false);

        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, 42);
        assert_eq!(errors[0].1, 162);
        // Head-ts / histogram / ticks / schedule / scanner / news / fundamental:
        // no bar-stream consumer waiting for historical_data_end.
        assert!(shared.reference.drain_historical_data().is_empty());
    }
}
