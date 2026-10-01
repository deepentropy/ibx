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
use crate::gateway::{ccp_reconnect_host, connect_farm, reconnect_ccp_via, CcpReconnect, ReconnectAuth};
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
    /// Connection states last reported to the clients; `None` until the
    /// first observation (ibx#399).
    links: Option<Links>,
    /// Market-data farm name, for the farm status messages.
    farm_name: String,
    // ── Subsystems ──
    pub(crate) farm: FarmState,
    pub(crate) ccp: CcpState,
    pub(crate) hmds: HmdsState,
    // ── Auto-reconnect ──
    reconnect_auth: Option<ReconnectAuth>,
    pending_farm_reconnect: Option<Receiver<io::Result<Connection>>>,
    farm_reconnect_attempt: u32,
    pending_ccp_reconnect: Option<Receiver<io::Result<CcpReconnect>>>,
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
            links: None,
            farm_name: "usfarm".to_string(),
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

    /// The session counts US stock sizes in round lots (ibx#287): market
    /// data sizes of those stocks are multiplied by the contract's lot.
    pub fn set_scale_us_lots(&mut self, on: bool) {
        self.context.scale_us_lots = on;
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

    /// Round lot of a new market data subscription (ibx#287). When the
    /// session counts US stock sizes in round lots and the contract may be
    /// one, its definition is asked for and the subscription waits for it,
    /// as the reference knows the contract before it subscribes: true when
    /// parked. Otherwise the known lot (or 1) is set now.
    fn park_for_round_lot(&mut self, sub: &farm::MdSubscribe) -> bool {
        let id = sub.instrument;
        if !self.context.scale_us_lots {
            self.context.market.set_round_lot(id, 1);
            return false;
        }
        if let Some(&lot) = self.context.round_lots.get(&sub.con_id) {
            self.context.market.set_round_lot(id, lot);
            return false;
        }
        let maybe_stock = matches!(sub.sec_type.to_ascii_uppercase().as_str(), "" | "STK" | "WAR");
        if !maybe_stock || sub.con_id <= 0 {
            self.context.market.set_round_lot(id, 1);
            return false;
        }
        if !self.context.lot_lookups.iter().any(|(_, c, _)| *c == sub.con_id) {
            let Some(conn) = self.ccp_conn.as_mut().filter(|_| !self.ccp.disconnected) else {
                log::warn!("No auth connection to read the round lot of con_id {}: subscribing with a round lot of 1", sub.con_id);
                self.context.market.set_round_lot(id, 1);
                return false;
            };
            let req_id = format!("ibxlot{}", self.context.next_lot_lookup);
            self.context.next_lot_lookup = self.context.next_lot_lookup.wrapping_add(1);
            let ts = chrono_free_timestamp();
            let con_id_str = sub.con_id.to_string();
            let exchange = match sub.exchange.to_ascii_uppercase().as_str() {
                "" | "SMART" => "BEST".to_string(),
                other => other.to_string(),
            };
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "c"),
                (fix::TAG_SENDING_TIME, &ts),
                (320, &req_id),
                (321, "2"),
                (146, "1"),
                (6008, &con_id_str),
                (6004, &exchange),
            ]);
            self.hb.last_ccp_sent = Instant::now();
            log::info!("Definition of con_id {} on {} asked for its round lot ({})", sub.con_id, exchange, req_id);
            self.context.lot_lookups.push((req_id, sub.con_id, Instant::now() + farm::LOT_LOOKUP_TIMEOUT));
        }
        self.context.lot_parked.push(sub.clone());
        true
    }

    /// Poll the auth socket once, then the timeouts of what waits on it,
    /// and send the market data subscriptions it released (ibx#287).
    #[inline]
    fn poll_auth(&mut self) {
        self.ccp.poll_executions(
            &mut self.ccp_conn, &mut self.context, &self.shared,
            &self.event_tx, &mut self.hb, &self.account_id,
        );
        self.ccp.sweep_pending_schedule_pairs(&self.shared, &self.event_tx);
        self.ccp.sweep_scanner_enrichments(&self.shared);
        self.ccp.sweep_contract_details(&self.shared, &self.event_tx, &mut self.ccp_conn, &mut self.hb);
        order_builder::sweep_rth_lookups(&mut self.context);
        farm::sweep_round_lot_lookups(&mut self.context);
        self.send_lot_ready();
        self.hmds.sweep_pending_historical(&self.shared);
    }

    /// Send the subscriptions whose round lot came in (ibx#287).
    fn send_lot_ready(&mut self) {
        if self.context.lot_ready.is_empty() { return; }
        for sub in std::mem::take(&mut self.context.lot_ready) {
            self.farm.send_md_subscribe(&sub, &mut self.farm_conn, &mut self.hb);
        }
    }

    /// Reclaim an instrument slot if nothing references it any more
    /// (ibx#233): no open orders, no market data subscription, no
    /// tick-by-tick subscription, no news subscription. A reused id would
    /// repoint those references at the wrong contract, so referenced slots
    /// stay resident until released. As the reference keeps a contract's
    /// market data while any observer still needs it, dropping one consumer
    /// leaves the others' data running (ibx#291).
    fn try_reclaim_instrument(&mut self, instrument: InstrumentId) {
        if !self.context.open_orders_for(instrument).is_empty() {
            return;
        }
        if self.farm.has_md_subscription(instrument)
            || self.context.lot_parked.iter().chain(&self.context.lot_ready).any(|s| s.instrument == instrument)
        {
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
            self.poll_auth();
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

            // 5c. Tell the clients about lost and restored links (ibx#399)
            self.report_link_changes();
            self.maybe_report_restored();

            // 6. Wake any waiting consumers (e.g. Python event loop)
            self.shared.notify();

            // 7. With every transport down there is nothing to poll, and the
            //    spin pinned a core for the whole outage (ibx#399). Park 1ms in
            //    that state only; reconnects run on a seconds-scale backoff and
            //    the connected path is unchanged.
            if self.all_transports_down() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    /// True when the farm and auth connections are down and no historical
    /// connection is up.
    fn all_transports_down(&self) -> bool {
        self.farm.disconnected
            && self.ccp.disconnected
            && (self.hmds_conn.is_none() || self.hmds.disconnected)
    }

    fn current_links(&self) -> Links {
        Links {
            ccp: !self.ccp.disconnected && self.ccp_conn.is_some(),
            farm: !self.farm.disconnected && self.farm_conn.is_some(),
            hmds: !self.hmds.disconnected && self.hmds_conn.is_some(),
        }
    }

    fn hmds_farm_name(&self) -> &str {
        self.reconnect_auth.as_ref()
            .map(|a| a.hmds_farm.as_str())
            .filter(|f| !f.is_empty())
            .unwrap_or("ushmds")
    }

    /// A lost link is reported to every client at once, as the reference
    /// does (ibx#399): 1100 for the auth connection, 2103 / 2105 for the
    /// market-data and historical farms. The clients stay connected. A farm
    /// that is up again after its logon gives 2104 / 2106 with its name, as
    /// in the reference; the auth link gives 1102 after its replay instead
    /// (`maybe_report_restored`).
    fn report_link_changes(&mut self) {
        let now = self.current_links();
        let Some(before) = self.links.replace(now) else { return };
        if before == now {
            return;
        }
        if before.ccp && !now.ccp {
            self.shared.push_connection_notice(1100, LINK_LOST.to_string());
        }
        if before.farm && !now.farm {
            self.shared.push_connection_notice(2103, format!("Market data farm connection is broken:{}", self.farm_name));
        }
        if !before.farm && now.farm {
            self.shared.push_connection_notice(2104, format!("Market data farm connection is OK:{}", self.farm_name));
        }
        if before.hmds && !now.hmds {
            let name = self.hmds_farm_name().to_string();
            self.shared.push_connection_notice(2105, format!("HMDS data farm connection is broken:{}", name));
        }
        if !before.hmds && now.hmds {
            let name = self.hmds_farm_name().to_string();
            self.shared.push_connection_notice(2106, format!("HMDS data farm connection is OK:{}", name));
        }
    }

    /// 1102 after a reconnect of the auth connection, once its order status
    /// replay has ended: at once when the data farms are up, else when they
    /// are up or after `RESTORE_FARM_WAIT`, with the farms that are not
    /// (ibx#399).
    fn maybe_report_restored(&mut self) {
        let Some(end_at) = self.ccp.status_replay_end_at else { return };
        let links = self.current_links();
        let hmds_expected = links.hmds
            || self.reconnect_auth.as_ref().is_some_and(|a| !a.hmds_host.is_empty());
        let all_up = links.farm && (links.hmds || !hmds_expected);
        if !all_up && end_at.elapsed() < RESTORE_FARM_WAIT {
            return;
        }
        self.ccp.status_replay_end_at = None;
        let mut farms = vec![(self.farm_name.clone(), links.farm)];
        if hmds_expected {
            farms.push((self.hmds_farm_name().to_string(), links.hmds));
        }
        let names = |up: bool| farms.iter().filter(|f| f.1 == up).map(|f| f.0.as_str()).collect::<Vec<_>>().join("; ");
        let message = if all_up {
            format!("{} All data farms are connected: {}.", LINK_RESTORED, names(true))
        } else {
            format!("{} The following farms are connected: {}. The following farms are not connected: {}.",
                LINK_RESTORED, names(true), names(false))
        };
        log::info!("Link restored: {}", message);
        self.shared.push_connection_notice(1102, message);
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
                        let sub = farm::MdSubscribe {
                            con_id, symbol, exchange, sec_type, last_trade_date, strike, right, multiplier,
                            instrument: id, mode_9887,
                        };
                        if !self.park_for_round_lot(&sub) {
                            self.farm.send_md_subscribe(&sub, &mut self.farm_conn, &mut self.hb);
                        }
                    }
                }
                ControlCommand::SetMarketDataType { market_data_type } => {
                    self.farm.market_data_type = market_data_type;
                }
                ControlCommand::Unsubscribe { instrument } => {
                    // Not sent yet: nothing to cancel on the farm.
                    self.context.lot_parked.retain(|s| s.instrument != instrument);
                    self.context.lot_ready.retain(|s| s.instrument != instrument);
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
                ControlCommand::FetchHistorical { req_id, con_id, symbol, sec_type, exchange, end_date_time, duration, bar_size, what_to_show, use_rth, keep_up_to_date } => {
                    // keepUpToDate sends via CCP but bars/end arrive on HMDS — both
                    // paths require an authed HMDS socket to deliver a completion.
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, true);
                    } else if keep_up_to_date {
                        if self.hmds.send_historical_request_via_ccp(req_id, con_id, &sec_type, &exchange, &end_date_time, &duration, &bar_size, &what_to_show, use_rth, &symbol, &mut self.ccp_conn, &mut self.hb, &self.ccp.ccp_sign_key, &self.ccp.ccp_sign_iv, &self.shared) {
                            self.hmds.keep_up_to_date_reqs.insert(req_id);
                        }
                    } else {
                        self.hmds.send_historical_request_ex(req_id, con_id, &sec_type, &exchange, &end_date_time, &duration, &bar_size, &what_to_show, use_rth, false, &symbol, &mut self.hmds_conn, &mut self.hb, &self.shared);
                    }
                }
                ControlCommand::CancelHistorical { req_id } => {
                    self.hmds.cancel_historical(req_id, &mut self.hmds_conn, &mut self.hb);
                }
                ControlCommand::FetchHeadTimestamp { req_id, con_id, sec_type, exchange, what_to_show, use_rth } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_head_timestamp_request(req_id, con_id, &sec_type, &exchange, &what_to_show, use_rth, &mut self.hmds_conn, &mut self.hb, &self.shared);
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
                ControlCommand::FetchHistogramData { req_id, con_id, sec_type, exchange, use_rth, period } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_histogram_request(req_id, con_id, &sec_type, &exchange, use_rth, &period, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::CancelHistogramData { req_id } => {
                    if let Some(pos) = self.hmds.pending_histogram.iter().position(|(_, rid)| *rid == req_id) {
                        self.hmds.pending_histogram.remove(pos);
                    }
                }
                ControlCommand::FetchHistoricalTicks { req_id, con_id, sec_type, exchange, start_date_time, end_date_time, number_of_ticks, what_to_show, use_rth } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_historical_ticks_request(req_id, con_id, &sec_type, &exchange, &start_date_time, &end_date_time, number_of_ticks, &what_to_show, use_rth, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::SubscribeRealTimeBar { req_id, con_id, symbol, sec_type, exchange, what_to_show, use_rth } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_realtime_bar_subscribe(req_id, con_id, &sec_type, &exchange, &symbol, &what_to_show, use_rth, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::CancelRealTimeBar { req_id } => {
                    if let Some(pos) = self.hmds.rtbar_subs.iter().position(|(_, rid, _, _)| *rid == req_id) {
                        let (query_id, _, ticker_id, _) = self.hmds.rtbar_subs.remove(pos);
                        let cancel_id = ticker_id.map(|t| t.to_string()).unwrap_or(query_id);
                        self.hmds.send_historical_cancel(&cancel_id, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::FetchHistoricalSchedule { req_id, con_id, sec_type, exchange, end_date_time, duration, use_rth } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_schedule_request(req_id, con_id, &sec_type, &exchange, &end_date_time, &duration, use_rth, &mut self.hmds_conn, &mut self.hb);
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
                ControlCommand::SetInstrumentCurrency { con_id, currency } => {
                    if let Some(id) = self.context.market.instrument_by_con_id(con_id) {
                        self.context.market.set_currency(id, &currency);
                    }
                }
                ControlCommand::SubscribeAccountSummary { sr_id, tags, group } => {
                    self.ccp.send_account_summary(&sr_id, Some((&tags, &group)), &mut self.ccp_conn, &mut self.hb);
                }
                ControlCommand::CancelAccountSummary { sr_id } => {
                    self.ccp.send_account_summary(&sr_id, None, &mut self.ccp_conn, &mut self.hb);
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
        let mut hmds_dead = false;
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
                    hmds_dead = true;
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
        // Drop the dead socket so the HMDS reconnect loop, which only runs
        // with no connection held, re-dials it (ibx#399).
        if hmds_dead {
            self.hmds_conn = None;
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

    /// Set the market-data farm name used in the farm status messages.
    pub fn set_farm_name(&mut self, name: String) {
        if !name.is_empty() {
            self.farm_name = name;
        }
    }

    /// Set cached auth credentials for farm auto-reconnect.
    pub fn set_reconnect_auth(&mut self, auth: ReconnectAuth) {
        self.reconnect_auth = Some(auth);
    }

    /// Whether auto-reconnect has a host to dial (ibx#399).
    pub fn has_reconnect_host(&self) -> bool {
        self.reconnect_auth.as_ref().is_some_and(|a| !a.host.is_empty())
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
                let delay = reconnect_backoff();
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
                let delay = reconnect_backoff();
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
        // The farm of the session, not a fixed name: a regional account logs
        // on again to its own farm, as in the reference (ibx#295).
        let (farm_host, farm_name) = farm_reconnect_target(&auth, &self.farm_name);
        log::info!("Farm auto-reconnect attempt {} starting (farm={}/{}, user={})",
            attempt, farm_host, farm_name, auth.username);

        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name(format!("farm-reconnect-{}", attempt))
            .spawn(move || {
                let result = connect_farm(
                    &farm_host, &farm_name,
                    &auth.username, &auth.password, auth.paper,
                    &auth.server_session_id, &auth.session_key,
                    &auth.hw_info, &auth.encoded, 18,
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
                // Retries continue on the backoff; the session stays open,
                // as with the reference, whose clients learn of the loss
                // from the farm status message (ibx#218, ibx#399).
                if self.farm_reconnect_attempt == 3 {
                    log::error!("Farm auto-reconnect failed 3 times (retries continue)");
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
        // The primary host and its backups in turn (ibx#399).
        let host = ccp_reconnect_host(&auth.host, attempt);
        log::info!("CCP auto-reconnect attempt {} starting (host={})", attempt, host);

        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name(format!("ccp-reconnect-{}", attempt))
            .spawn(move || {
                let _ = tx.send(reconnect_ccp_via(&auth, &host));
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
            Ok(Ok(CcpReconnect { conn, session_epoch })) => {
                log::info!("CCP auto-reconnect succeeded (attempt {})", self.ccp_reconnect_attempt);
                // The next reconnect resumes this server session (ibx#422).
                if let (Some(epoch), Some(auth)) = (session_epoch, self.reconnect_auth.as_mut()) {
                    auth.session_epoch = epoch;
                }
                self.reconnect_ccp(conn);
                self.ccp_reconnect_attempt = 0;
                self.ccp_next_attempt_at = None;
                self.hb.ccp_up_since = Instant::now();
                self.pending_ccp_reconnect = None;
            }
            Ok(Err(e)) => {
                log::error!("CCP auto-reconnect failed (attempt {}): {}", self.ccp_reconnect_attempt, e);
                self.pending_ccp_reconnect = None;
                // See the farm path: the clients had the lost-link message
                // at once and stay connected (ibx#399).
                if self.ccp_reconnect_attempt == 3 {
                    log::error!("CCP auto-reconnect failed 3 times (retries continue)");
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
                    &auth.hw_info, &auth.encoded, 17,
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

    /// Test-only: lose the farm connection through the same path as a real
    /// loss (subscription state cleared, socket dropped).
    pub fn lose_farm_for_test(&mut self) {
        self.farm.handle_disconnect(&mut self.context, &self.event_tx);
        self.farm_conn = None;
    }

    /// Test-only: the engine's instrument table.
    pub fn market_for_test(&mut self) -> &mut crate::engine::market_state::MarketState {
        &mut self.context.market
    }

    /// Test-only: poll the farm socket once.
    pub fn poll_farm_for_test(&mut self) {
        self.farm.poll_market_data(
            &mut self.farm_conn, &mut self.context, &self.shared,
            &self.event_tx, &mut self.hb,
        );
    }

    /// Test-only: poll the auth socket once, as step 3 of the loop does.
    pub fn poll_auth_for_test(&mut self) {
        self.poll_auth();
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
            crate::types::Side::Buy => fill.qty_fixed,
            crate::types::Side::Sell | crate::types::Side::ShortSell => -fill.qty_fixed,
        };
        self.context.update_position_fixed(fill.instrument, delta);
        self.shared.orders.push_fill(*fill);
        self.shared.portfolio.set_position_fixed(fill.instrument, self.context.position_fixed(fill.instrument));
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

/// Jittered reconnect delay for CCP/farm: 5 s plus up to 10 s, the same
/// for every attempt, as the reference retries a lost link every 5 to 15 s
/// (ibx#399; ibx#218 for the jitter). Immediate rapid-fire re-dials risk
/// server-side rate limiting. (HMDS keeps its own schedule below.)
pub(crate) fn reconnect_backoff() -> std::time::Duration {
    const FLOOR_MS: u64 = 5_000;
    const JITTER_MS: u64 = 10_000;
    std::time::Duration::from_millis(FLOOR_MS + rand::random::<u64>() % JITTER_MS)
}

/// Host and name for a market-data farm reconnect: the session's farm, else
/// the auth host and `fallback_name` (the name used at login).
fn farm_reconnect_target(auth: &ReconnectAuth, fallback_name: &str) -> (String, String) {
    let host = if auth.farm_host.is_empty() { auth.host.clone() } else { auth.farm_host.clone() };
    let name = if auth.farm_name.is_empty() { fallback_name.to_string() } else { auth.farm_name.clone() };
    (host, name)
}

/// Up/down state of each connection, as reported to the clients.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Links {
    ccp: bool,
    farm: bool,
    hmds: bool,
}

/// Longest wait for the data farms before the restored-link message.
const RESTORE_FARM_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

const LINK_LOST: &str = "Connectivity between client and server has been lost.";
const LINK_RESTORED: &str = "Connectivity between client and server has been restored - data maintained.";

/// Backoff schedule for HMDS reconnect attempts (ibx#187, ib-agent#153).
/// `min(64, 3 * 2^(attempt-1))` seconds — approximates the captured cadence
/// of 3.2 / 11.4 / 18.5 / 42.7 / 63.7 s the official client uses.
#[inline]
pub(crate) fn hmds_reconnect_backoff(attempt: u32) -> std::time::Duration {
    let n = attempt.saturating_sub(1).min(31);
    let secs = (3u64.saturating_mul(1u64 << n)).min(64);
    std::time::Duration::from_secs(secs)
}

/// Surface an "HMDS unavailable" error for `req_id` when the historical-data
/// socket isn't connected: code 162
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
    // Between -1 and 0 the whole part truncates to 0 and cannot carry the sign.
    if price < 0 && whole == 0 {
        s.push(b'-');
    }
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

/// A price in the reference's number form for its price fields: at least
/// two decimals, at most eight (`0.00`, `0.05`, `272.885`). Zero alloc.
pub(crate) fn format_price_ref(price: Price) -> StackStr {
    let mut s = format_price(price);
    let len = s.len as usize;
    match s.buf[..len].iter().position(|&b| b == b'.') {
        None => { s.push(b'.'); s.push(b'0'); s.push(b'0'); }
        Some(dot) if len - dot == 2 => s.push(b'0'),
        Some(_) => {}
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

/// The time in force of a code, as the reference reports it (ibx#307):
/// every code it knows by its text, "???" for any other. The inverse of
/// `api::types::Order::tif_byte` for the values ibx sends (ibx#220).
pub(crate) fn decode_tif(tif: u8) -> &'static str {
    match tif {
        b'0' => "DAY", b'1' => "GTC", b'2' => "OPG", b'3' => "IOC",
        b'4' => "FOK", b'5' => "GTX", b'6' => "GTD", b'8' => "AUC",
        b'?' => "[INVALID]", b'b' => "OVERNIGHT + DAY", b'j' => "OVERNIGHT",
        b'p' => "Minutes", crate::types::TIF_DTC => "DTC", _ => "???",
    }
}

/// The time-in-force code of an order report, as the reference reads it
/// (ibx#307): DAY when 59 is absent; else the first character of 59, DTC
/// for 59=1 with the DTC flag (6436=1), OVERNIGHT + DAY with 8534=1, else
/// OVERNIGHT when the exchange (6004) is OVERNIGHT.
pub(crate) fn report_tif(parsed: &std::collections::HashMap<u32, String>) -> u8 {
    let Some(mut code) = parsed.get(&59).and_then(|s| s.bytes().next()) else { return b'0' };
    let flag = |tag: u32| parsed.get(&tag).map(|s| s.as_str()) == Some("1");
    if code == b'1' && flag(6436) { code = crate::types::TIF_DTC; }
    if flag(8534) {
        code = b'b';
    } else if parsed.get(&6004).map(|s| s.as_str()) == Some("OVERNIGHT") {
        code = b'j';
    }
    code
}

/// Parse a decimal quantity ("1", "0.5") into a fixed-point Qty
/// (QTY_SCALE = 10^4). A fraction such as a partial share is kept: reading
/// it as a whole number dropped the fill (ibx#313).
pub(crate) fn parse_qty(s: &str) -> Option<Qty> {
    s.parse::<f64>().ok().filter(|v| v.is_finite()).map(|v| (v * QTY_SCALE as f64).round() as Qty)
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

    // ibx#332: a price between -1 and 0 lost its minus sign.
    #[test]
    fn format_price_keeps_sign_between_minus_one_and_zero() {
        let p = |v: f64| format_price((v * PRICE_SCALE as f64).round() as Price);
        assert_eq!(&*p(-0.30), "-0.3");
        assert_eq!(&*p(-0.05), "-0.05");
        assert_eq!(&*p(-0.01), "-0.01");
        assert_eq!(&*p(-1.25), "-1.25");
        assert_eq!(&*p(-1.0), "-1");
        assert_eq!(&*p(0.0), "0");
        assert_eq!(&*p(0.35), "0.35");
        assert_eq!(&*p(1.5), "1.5");
        assert_eq!(&*format_price(-1), "-0.00000001");
    }

    // The reference writes its price fields with two decimals at least.
    #[test]
    fn format_price_ref_has_two_decimals_at_least() {
        let p = |v: f64| format_price_ref((v * PRICE_SCALE as f64).round() as Price);
        assert_eq!(&*p(0.0), "0.00");
        assert_eq!(&*p(0.05), "0.05");
        assert_eq!(&*p(0.5), "0.50");
        assert_eq!(&*p(-0.5), "-0.50");
        assert_eq!(&*p(272.88), "272.88");
        assert_eq!(&*p(721.0), "721.00");
        assert_eq!(&*p(272.885), "272.885");
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
            cum_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_price: 0,
            instrument: 0,
            order_id: 1001,
            side: Side::Buy,
            price: 150_00000000,
            qty_fixed: (100) as i64 * crate::types::QTY_SCALE,
            remaining_fixed: (0) as i64 * crate::types::QTY_SCALE,
            commission: 1_00000000,
            timestamp_ns: 0,
        };
        engine.inject_fill(&fill);
        assert_eq!(engine.context_mut().position_fixed(0) / crate::types::QTY_SCALE, 100);
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

    // ibx#399: every CCP/farm reconnect waits 5 to 15 s, as the reference.
    #[test]
    fn reconnect_backoff_is_five_to_fifteen_seconds() {
        use std::time::Duration;
        let (mut lo, mut hi) = (Duration::MAX, Duration::ZERO);
        for _ in 0..2_000 {
            let d = reconnect_backoff();
            assert!(d >= Duration::from_secs(5) && d < Duration::from_secs(15), "got {:?}", d);
            lo = lo.min(d);
            hi = hi.max(d);
        }
        assert!(lo < Duration::from_secs(7) && hi > Duration::from_secs(13), "jittered over the range: {:?}..{:?}", lo, hi);
    }

    fn loopback_conn() -> (Connection, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (Connection::new_raw(client).unwrap(), server)
    }

    fn engine_with_links() -> (HotLoop, Arc<SharedState>, Vec<std::net::TcpStream>) {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_reconnect_auth(reconnect_auth_with_host("gw.example"));
        engine.set_farm_name("usfarm".into());
        let (ccp, s1) = loopback_conn();
        let (farm, s2) = loopback_conn();
        let (hmds, s3) = loopback_conn();
        engine.ccp_conn = Some(ccp);
        engine.farm_conn = Some(farm);
        engine.hmds_conn = Some(hmds);
        (engine, shared, vec![s1, s2, s3])
    }

    // ibx#399: a lost link is told to the clients at once, once, and the
    // session stays open.
    #[test]
    fn lost_links_are_reported_at_once() {
        let (mut engine, shared, _servers) = engine_with_links();
        engine.report_link_changes();
        assert!(shared.drain_connection_notices().is_empty(), "first look: nothing to report");

        engine.ccp.disconnected = true;
        engine.farm.disconnected = true;
        engine.hmds.disconnected = true;
        engine.report_link_changes();
        assert_eq!(shared.drain_connection_notices(), vec![
            (1100, "Connectivity between client and server has been lost.".to_string()),
            (2103, "Market data farm connection is broken:usfarm".to_string()),
            (2105, "HMDS data farm connection is broken:ushmds".to_string()),
        ]);
        engine.report_link_changes();
        assert!(shared.drain_connection_notices().is_empty(), "reported once");
        assert!(!shared.take_connection_lost(), "the session is not closed");
    }

    // ibx#399: 1102 after the status replay end, at once with the farms up.
    #[test]
    fn restored_link_is_reported_after_the_status_replay() {
        let (mut engine, shared, _servers) = engine_with_links();
        engine.maybe_report_restored();
        assert!(shared.drain_connection_notices().is_empty(), "no reconnect, no message");

        engine.ccp.status_replay_end_at = Some(Instant::now());
        engine.maybe_report_restored();
        assert_eq!(shared.drain_connection_notices(), vec![(1102,
            "Connectivity between client and server has been restored - data maintained. All data farms are connected: usfarm; ushmds.".to_string())]);
        engine.maybe_report_restored();
        assert!(shared.drain_connection_notices().is_empty(), "reported once");
    }

    // With a farm still down the message waits for it, 30 s at most.
    #[test]
    fn restored_link_waits_for_the_farms() {
        let (mut engine, shared, _servers) = engine_with_links();
        engine.farm.disconnected = true;
        engine.ccp.status_replay_end_at = Some(Instant::now());
        engine.maybe_report_restored();
        assert!(shared.drain_connection_notices().is_empty(), "farm down: wait");

        engine.ccp.status_replay_end_at = Instant::now().checked_sub(RESTORE_FARM_WAIT);
        engine.maybe_report_restored();
        assert_eq!(shared.drain_connection_notices(), vec![(1102,
            "Connectivity between client and server has been restored - data maintained. The following farms are connected: ushmds. The following farms are not connected: usfarm.".to_string())]);
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

    fn reconnect_auth_with_host(host: &str) -> ReconnectAuth {
        ReconnectAuth {
            host: host.into(),
            username: "user".into(),
            password: zeroize::Zeroizing::new("pass".into()),
            paper: true,
            session_key: num_bigint::BigUint::default(),
            session_token: num_bigint::BigUint::default(),
            server_session_id: String::new(),
            hw_info: String::new(),
            encoded: String::new(),
            hmds_host: "hmds.example".into(),
            hmds_farm: "ushmds".into(),
            farm_host: String::new(),
            farm_name: String::new(),
            session_epoch: String::new(),
        }
    }

    // ibx#295: the farm reconnect goes to the session's farm (host and name
    // from the logon), not to the auth host under a fixed name.
    #[test]
    fn farm_reconnect_uses_the_session_farm() {
        let mut auth = reconnect_auth_with_host("gw.example");
        auth.farm_host = "zdc1.example".into();
        auth.farm_name = "eufarm".into();
        assert_eq!(farm_reconnect_target(&auth, "usfarm"), ("zdc1.example".to_string(), "eufarm".to_string()));
        // No farm parsed: the auth host and the login name, as at login.
        let auth = reconnect_auth_with_host("gw.example");
        assert_eq!(farm_reconnect_target(&auth, "usfarm.nj"), ("gw.example".to_string(), "usfarm.nj".to_string()));
    }

    // ibx#399: a farm that is up again gives the farm-OK notice with its
    // name; the first look reports nothing.
    #[test]
    fn restored_farms_are_reported_with_their_names() {
        let (mut engine, shared, _servers) = engine_with_links();
        engine.set_farm_name("eufarm".into());
        engine.report_link_changes();
        assert!(shared.drain_connection_notices().is_empty(), "first look: nothing to report");

        engine.farm.disconnected = true;
        engine.hmds.disconnected = true;
        engine.report_link_changes();
        let _ = shared.drain_connection_notices();

        engine.farm.disconnected = false;
        engine.hmds.disconnected = false;
        engine.report_link_changes();
        assert_eq!(shared.drain_connection_notices(), vec![
            (2104, "Market data farm connection is OK:eufarm".to_string()),
            (2106, "HMDS data farm connection is OK:ushmds".to_string()),
        ]);
        engine.report_link_changes();
        assert!(shared.drain_connection_notices().is_empty(), "reported once");

        // The auth link coming back gives no farm notice.
        engine.ccp.disconnected = true;
        engine.report_link_changes();
        let _ = shared.drain_connection_notices();
        engine.ccp.disconnected = false;
        engine.report_link_changes();
        assert!(shared.drain_connection_notices().is_empty(), "1102 comes after the replay, not here");
    }

    // ibx#399: with every transport down the loop spun at ~1M passes/s and
    // pinned a core for the whole outage. Parked, 60ms is ~60 passes.
    #[test]
    fn a_loop_with_every_transport_down_does_not_spin() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared, None, None);
        engine.set_control_rx(rx);
        engine.force_farm_disconnect();
        engine.ccp.disconnected = true;
        assert!(engine.all_transports_down());

        let handle = std::thread::spawn(move || { engine.run(); engine });
        std::thread::sleep(Duration::from_millis(60));
        tx.send(ControlCommand::Shutdown).unwrap();
        let engine = handle.join().unwrap();
        let passes = engine.context.loop_iterations;
        assert!(passes < 1_000, "loop spun {} times in 60ms while every transport was down", passes);
    }

    // ibx#399: the park applies only when nothing is up.
    #[test]
    fn a_loop_with_any_transport_up_is_not_parked() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared, None, None);
        assert!(!engine.all_transports_down(), "all up");
        engine.force_farm_disconnect();
        assert!(!engine.all_transports_down(), "auth still up");
        engine.farm.disconnected = false;
        engine.ccp.disconnected = true;
        assert!(!engine.all_transports_down(), "farm still up");
    }

    // ibx#399: a mid-session historical loss set the flag but kept the dead
    // socket, and the reconnect loop only runs with no socket held, so the
    // historical connection never came back.
    #[test]
    fn a_lost_hmds_socket_is_dropped_and_reconnect_is_scheduled() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        drop(server);

        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_reconnect_auth(reconnect_auth_with_host("gw.example"));
        engine.hmds_conn = Some(Connection::new_raw(client).unwrap());

        let deadline = Instant::now() + Duration::from_secs(2);
        while !engine.hmds.disconnected && Instant::now() < deadline {
            engine.hmds.poll(&mut engine.hmds_conn, &shared, &None, &mut engine.hb);
        }
        assert!(engine.hmds.disconnected, "peer close must be detected");
        assert!(engine.hmds_conn.is_none(), "dead socket must be dropped");

        engine.maybe_spawn_hmds_reconnect();
        assert!(engine.hmds_next_attempt_at.is_some(), "reconnect must be scheduled");
    }

    /// A keyed connection and its server side, plus a frame the server
    /// signed whose signature value was then changed.
    fn conn_with_bad_signed_frame() -> (Connection, std::net::TcpStream, Vec<u8>) {
        let (client, server) = socket_pair();
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let mut conn = Connection::new_raw(client).unwrap();
        conn.set_keys(Vec::new(), Vec::new(), mac_key.clone(), iv.clone());
        let (mut signed, _) = fix::fix_sign(&fix::fix_build(&[(35, "0")], 1), &mac_key, &iv);
        let pos = signed.windows(5).position(|w| w == b"8349=").unwrap() + 5;
        signed[pos] = if signed[pos] == b'0' { b'1' } else { b'0' };
        (conn, server, signed)
    }

    // ibx#275: a signature mismatch on the market-data farm drops the
    // connection and schedules the normal reconnect.
    #[test]
    fn a_farm_signature_mismatch_drops_the_connection() {
        use std::io::{Read, Write};
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared, None, None);
        engine.set_reconnect_auth(reconnect_auth_with_host("gw.example"));
        let (conn, mut server, bad) = conn_with_bad_signed_frame();
        engine.farm_conn = Some(conn);
        server.write_all(&bad).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while !engine.farm.disconnected && Instant::now() < deadline {
            engine.poll_farm_for_test();
        }
        assert!(engine.farm.disconnected, "mismatch must drop the farm");
        // The socket is closed: the server side reads end of stream.
        server.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(server.read(&mut buf).unwrap_or(0), 0, "socket closed");
        engine.maybe_spawn_farm_reconnect();
        assert!(engine.farm_next_attempt_at.is_some(), "reconnect scheduled");
    }

    // ibx#275: same on the historical connection: the socket is dropped and
    // the reconnect loop re-dials it.
    #[test]
    fn an_hmds_signature_mismatch_drops_the_connection() {
        use std::io::Write;
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_reconnect_auth(reconnect_auth_with_host("gw.example"));
        let (conn, mut server, bad) = conn_with_bad_signed_frame();
        engine.hmds_conn = Some(conn);
        server.write_all(&bad).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while !engine.hmds.disconnected && Instant::now() < deadline {
            engine.hmds.poll(&mut engine.hmds_conn, &shared, &None, &mut engine.hb);
        }
        assert!(engine.hmds.disconnected, "mismatch must drop the historical connection");
        assert!(engine.hmds_conn.is_none(), "socket dropped");
        engine.maybe_spawn_hmds_reconnect();
        assert!(engine.hmds_next_attempt_at.is_some(), "reconnect scheduled");
    }

    fn socket_pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    /// Every compressed message the engine wrote to `server`, as inner text.
    fn farm_messages_sent(server: &mut std::net::TcpStream) -> Vec<String> {
        use std::io::Read;
        server.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = server.read(&mut chunk) {
            if n == 0 { break; }
            buf.extend_from_slice(&chunk[..n]);
        }
        let mut out = Vec::new();
        let mut rest = &buf[..];
        while let Some(len) = crate::protocol::fixcomp::fixcomp_length(rest) {
            for m in crate::protocol::fixcomp::fixcomp_decompress(&rest[..len]).unwrap() {
                out.push(String::from_utf8_lossy(&m).replace('\x01', "|"));
            }
            rest = &rest[len..];
        }
        out
    }

    /// Every plain message the engine wrote to `server`, `|` separated.
    fn plain_messages_sent(server: &mut std::net::TcpStream) -> Vec<String> {
        use std::io::Read;
        server.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = server.read(&mut chunk) {
            if n == 0 { break; }
            buf.extend_from_slice(&chunk[..n]);
        }
        String::from_utf8_lossy(&buf).replace('\x01', "|")
            .split("8=FIX").filter(|m| !m.is_empty()).map(|m| format!("8=FIX{m}")).collect()
    }

    fn subscribe_cmd(con_id: i64, sec_type: &str) -> ControlCommand {
        ControlCommand::Subscribe {
            con_id, symbol: String::new(), exchange: "SMART".into(), sec_type: sec_type.into(),
            last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
            mode_9887: 0, reply_tx: None,
        }
    }

    // ibx#287: with the lots scaling on, a stock subscription waits for the
    // contract's definition and goes out once the round lot is known; a
    // second subscription of the contract uses the kept lot at once.
    #[test]
    fn stock_subscription_waits_for_its_round_lot() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (c1, mut farm_side) = socket_pair();
        let (c2, mut ccp_side) = socket_pair();
        engine.farm_conn = Some(Connection::new_raw(c1).unwrap());
        engine.ccp_conn = Some(Connection::new_raw(c2).unwrap());
        engine.set_scale_us_lots(true);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);

        tx.send(subscribe_cmd(265598, "STK")).unwrap();
        engine.poll_once();
        let asked = plain_messages_sent(&mut ccp_side);
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert!(asked[0].contains("|35=c|") && asked[0].contains("|6008=265598|") && asked[0].contains("|6004=BEST|"), "{}", asked[0]);
        let req_id = asked[0].split('|').find_map(|p| p.strip_prefix("320=")).unwrap().to_string();
        assert!(farm_messages_sent(&mut farm_side).is_empty(), "nothing subscribed before the lot is known");

        let reply = fix::fix_build(&[(35, "d"), (320, &req_id), (6008, "265598"), (167, "CS"),
            (6523, "USSTK"), (6030, "1"), (6023, "40"), (6027, "40")], 1);
        assert!(farm::round_lot_reply(&mut engine.context, &req_id, &reply));
        engine.send_lot_ready();
        let sent = farm_messages_sent(&mut farm_side);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].contains("6008=265598|"), "{}", sent[0]);
        let id = engine.context.market.instrument_by_con_id(265598).unwrap();
        assert_eq!(engine.context.market.round_lot(id), 40);

        // Cancelled and asked again: the lot is kept, no second lookup.
        tx.send(ControlCommand::Unsubscribe { instrument: id }).unwrap();
        tx.send(subscribe_cmd(265598, "STK")).unwrap();
        engine.poll_once();
        assert!(plain_messages_sent(&mut ccp_side).is_empty());
        let id = engine.context.market.instrument_by_con_id(265598).unwrap();
        assert_eq!(engine.context.market.round_lot(id), 40);
        let sent = farm_messages_sent(&mut farm_side);
        assert_eq!(sent.len(), 3, "two cancels, then the new subscription: {sent:?}");
        assert!(sent[2].contains("6008=265598|"), "{}", sent[2]);
    }

    // ibx#291: dropping the tick-by-tick or news consumer of a contract
    // keeps the slot while its market data runs, also while the farm is
    // down; the market data cancel then frees it.
    #[test]
    fn a_live_market_data_subscription_keeps_its_slot() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        tx.send(subscribe_cmd(265598, "STK")).unwrap();
        tx.send(ControlCommand::SubscribeTbt { con_id: 265598, symbol: String::new(), tbt_type: crate::types::TbtType::BidAsk, reply_tx: None }).unwrap();
        tx.send(ControlCommand::SubscribeNews { con_id: 265598, symbol: String::new(), providers: String::new(), reply_tx: None }).unwrap();
        engine.poll_once();
        let id = engine.context.market.instrument_by_con_id(265598).unwrap();

        tx.send(ControlCommand::UnsubscribeTbt { instrument: id }).unwrap();
        tx.send(ControlCommand::UnsubscribeNews { instrument: id }).unwrap();
        engine.poll_once();
        assert_eq!(engine.context.market.con_id(id), Some(265598));

        // Farm down: the subscription waits for the reconnect, the slot stays.
        engine.farm.handle_disconnect(&mut engine.context, &None);
        tx.send(ControlCommand::SubscribeTbt { con_id: 265598, symbol: String::new(), tbt_type: crate::types::TbtType::BidAsk, reply_tx: None }).unwrap();
        tx.send(ControlCommand::UnsubscribeTbt { instrument: id }).unwrap();
        engine.poll_once();
        assert_eq!(engine.context.market.con_id(id), Some(265598));

        tx.send(ControlCommand::Unsubscribe { instrument: id }).unwrap();
        engine.poll_once();
        assert_eq!(engine.context.market.con_id(id), None, "freed once nothing uses it");
    }

    // ibx#287: no lookup for a contract that is not a stock, or when the
    // session does not scale lots; a subscription cancelled while it
    // waits is never sent.
    #[test]
    fn round_lot_lookup_only_where_it_can_apply() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (c1, mut farm_side) = socket_pair();
        let (c2, mut ccp_side) = socket_pair();
        engine.farm_conn = Some(Connection::new_raw(c1).unwrap());
        engine.ccp_conn = Some(Connection::new_raw(c2).unwrap());
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);

        // Scaling off: sent at once, lot 1.
        tx.send(subscribe_cmd(265598, "STK")).unwrap();
        engine.poll_once();
        assert!(plain_messages_sent(&mut ccp_side).is_empty());
        assert_eq!(farm_messages_sent(&mut farm_side).len(), 1);

        // Scaling on, a future: sent at once, lot 1.
        engine.set_scale_us_lots(true);
        tx.send(subscribe_cmd(551601503, "FUT")).unwrap();
        engine.poll_once();
        assert!(plain_messages_sent(&mut ccp_side).is_empty());
        assert_eq!(farm_messages_sent(&mut farm_side).len(), 1);
        let fut = engine.context.market.instrument_by_con_id(551601503).unwrap();
        assert_eq!(engine.context.market.round_lot(fut), 1);

        // A stock cancelled while it waits: nothing reaches the farm.
        tx.send(subscribe_cmd(272093, "STK")).unwrap();
        engine.poll_once();
        assert_eq!(plain_messages_sent(&mut ccp_side).len(), 1);
        let msft = engine.context.market.instrument_by_con_id(272093).unwrap();
        tx.send(ControlCommand::Unsubscribe { instrument: msft }).unwrap();
        engine.poll_once();
        assert!(engine.context.lot_parked.is_empty());
        engine.context.lot_lookups[0].2 = Instant::now();
        farm::sweep_round_lot_lookups(&mut engine.context);
        engine.send_lot_ready();
        assert!(farm_messages_sent(&mut farm_side).is_empty());
    }

    // ibx#288: handle_disconnect cleared the request-id maps and reconnect
    // rebuilt its list from them, so no subscription came back after a farm
    // reconnect. A subscription cancelled while the farm was down must still
    // stay cancelled.
    #[test]
    fn farm_reconnect_reissues_every_live_subscription() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared, None, None);
        let (c1, _s1) = socket_pair();
        engine.farm_conn = Some(Connection::new_raw(c1).unwrap());
        let aapl = engine.context.market.register(265598);
        let msft = engine.context.market.register(272093);
        let spy = engine.context.market.register(756733);
        for (con_id, sym, inst, mode) in [(265598, "AAPL", aapl, 0), (272093, "MSFT", msft, 0), (756733, "SPY", spy, 3)] {
            engine.farm.send_mktdata_subscribe(
                con_id, sym, "SMART", "STK", "", 0.0, "", "", inst, mode,
                &mut engine.farm_conn, &mut engine.hb,
            );
        }

        engine.farm.handle_disconnect(&mut engine.context, &None);
        engine.farm.send_mktdata_unsubscribe(msft, &mut engine.farm_conn, &mut engine.hb);

        let (c2, mut s2) = socket_pair();
        engine.reconnect_farm(Connection::new_raw(c2).unwrap());

        let sent = farm_messages_sent(&mut s2);
        assert_eq!(sent.len(), 2, "one subscribe per live instrument: {:?}", sent);
        let aapl_sub = sent.iter().find(|m| m.contains("6008=265598")).expect("AAPL re-subscribed");
        assert!(aapl_sub.contains("264=442|") && aapl_sub.contains("264=443|"), "realtime keeps both entries");
        let spy_sub = sent.iter().find(|m| m.contains("6008=756733")).expect("SPY re-subscribed");
        assert!(spy_sub.contains("9887=3|"), "delayed mode kept: {}", spy_sub);
        assert!(!sent.iter().any(|m| m.contains("6008=272093")), "MSFT was cancelled while down");
        assert_eq!(engine.farm.instrument_md_reqs.len(), 2);
        // Every subscription is the 442 / 443 pair, delayed too (ibx#447).
        assert_eq!(engine.farm.md_req_to_instrument.len(), 4);

        // A second drop and reconnect re-issues them again.
        engine.farm.handle_disconnect(&mut engine.context, &None);
        let (c3, mut s3) = socket_pair();
        engine.reconnect_farm(Connection::new_raw(c3).unwrap());
        assert_eq!(farm_messages_sent(&mut s3).len(), 2);
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

    // ibx#447 (captured 28/09/2026): the subscribe is the 442 / 443 pair
    // with no 9839; with delayed data enabled, a reject with delayed data
    // available asks again on new ids with 9887=1 on each entry, and the
    // client learns it (marketDataType 3, 10167). Without it, the
    // subscription stops (354 with the "delayed available" text).
    #[test]
    fn a_rejected_subscription_goes_delayed_or_stops() {
        for market_data_type in [3, 1] {
            let shared = Arc::new(SharedState::new());
            let mut engine = HotLoop::new(shared.clone(), None, None);
            let (c1, mut s1) = socket_pair();
            engine.farm_conn = Some(Connection::new_raw(c1).unwrap());
            engine.farm.market_data_type = market_data_type;
            let jp = engine.context.market.register(13905804);
            engine.farm.send_mktdata_subscribe(13905804, "7203", "SMART", "STK", "", 0.0, "", "", jp, 0,
                &mut engine.farm_conn, &mut engine.hb);
            let first = farm_messages_sent(&mut s1);
            assert_eq!(first.len(), 1);
            assert!(first[0].contains("264=442|") && first[0].contains("264=443|"), "{}", first[0]);
            assert!(!first[0].contains("9839=") && !first[0].contains("9887="), "{}", first[0]);
            let ids: Vec<String> = engine.farm.md_req_to_instrument.iter().map(|(r, _)| r.to_string()).collect();

            let reject = crate::protocol::fix::fix_build(&[(35, "3"), (45, "0"),
                (58, "Error&BEST/STK/Top&BEST/STK/Top"), (262, &ids.join(";")), (9887, "1;1"),
                (6756, "133,134;133,134"), (9888, "1;1")], 1);
            engine.farm.process_farm_message(&reject, &mut engine.farm_conn, &mut engine.context,
                &shared, &None, &mut engine.hb);

            let rejects = shared.market.drain_md_rejects();
            if market_data_type == 3 {
                let again = farm_messages_sent(&mut s1);
                assert_eq!(again.len(), 1, "{again:?}");
                assert_eq!(again[0].matches("9887=1|").count(), 2, "{}", again[0]);
                assert!(!ids.iter().any(|id| again[0].contains(&format!("262={}|", id))), "new ids: {}", again[0]);
                assert_eq!(rejects, [crate::bridge::MdReject::Delayed { instrument: jp }]);
                assert_eq!(engine.farm.instrument_md_reqs[0].1.len(), 4, "a cancel covers the rejected ids too");
            } else {
                assert!(farm_messages_sent(&mut s1).is_empty());
                assert_eq!(rejects, [crate::bridge::MdReject::NotSubscribed {
                    instrument: jp, delayed_available: true, needs_api_subscription: false }]);
                assert!(engine.farm.instrument_md_reqs.is_empty());
                assert!(engine.farm.md_req_to_instrument.is_empty());
            }
        }
    }
}
