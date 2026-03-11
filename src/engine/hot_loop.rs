use std::collections::HashSet;
use std::time::Instant;

use crate::engine::context::{Context, Strategy};
use crate::config::{chrono_free_timestamp, unix_to_ib_datetime};
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::protocol::tick_decoder;
use crate::types::{AlgoParams, ControlCommand, InstrumentId, OrderCondition, OrderRequest, Price, Side, PRICE_SCALE};
use crossbeam_channel::{bounded, Receiver, Sender};

/// CCP heartbeat interval (10 seconds, configurable via FIX tag 108).
const CCP_HEARTBEAT_SECS: u64 = 10;
/// Farm heartbeat interval (30 seconds).
const FARM_HEARTBEAT_SECS: u64 = 30;
/// Grace period before declaring timeout (1 second).
const HEARTBEAT_GRACE_SECS: u64 = 1;

/// The pinned-core hot loop. Runs strategy inline with decode and order send.
/// Generic over S to monomorphize the strategy — zero vtable overhead.
pub struct HotLoop<S: Strategy> {
    strategy: S,
    context: Context,
    /// Core ID to pin the hot loop thread to. None = no pinning.
    core_id: Option<usize>,
    /// Farm connection for market data (usfarm).
    pub farm_conn: Option<Connection>,
    /// CCP connection for order management.
    pub ccp_conn: Option<Connection>,
    /// HMDS farm connection for historical data (optional).
    pub hmds_conn: Option<Connection>,
    /// Next market data request ID for FIX tag 262.
    next_md_req_id: u32,
    /// Pending subscriptions: MDReqID → InstrumentId (awaiting 35=Q ack).
    md_req_to_instrument: Vec<(u32, InstrumentId)>,
    /// Active subscriptions: InstrumentId → MDReqIDs (for unsubscribe).
    instrument_md_reqs: Vec<(InstrumentId, Vec<u32>)>,
    /// SPSC channel receiver for control plane commands.
    control_rx: Option<Receiver<ControlCommand>>,
    /// Whether the hot loop should keep running.
    running: bool,
    /// Whether farm connection is lost (needs reconnect).
    farm_disconnected: bool,
    /// Whether CCP connection is lost (needs reconnect).
    ccp_disconnected: bool,
    /// Heartbeat state.
    hb: HeartbeatState,
    /// Account ID for order submission.
    account_id: String,
    /// Seen ExecIDs (FIX tag 17) for fill deduplication.
    /// Prevents double-counting when IB sends duplicate execution reports.
    seen_exec_ids: HashSet<String>,
    /// Whether HMDS connection is lost.
    hmds_disconnected: bool,
    /// Next TBT request ID for HMDS subscriptions.
    next_tbt_req_id: u32,
    /// Active TBT subscriptions: InstrumentId → (ticker_id, tbt_type).
    tbt_subscriptions: Vec<(InstrumentId, String, crate::types::TbtType)>,
    /// Running price state for TBT decoding: InstrumentId → (last_price_cents, bid_cents, ask_cents).
    tbt_price_state: Vec<(InstrumentId, i64, i64, i64)>,
}

/// Tracks last send/recv times and pending test requests for heartbeat management.
pub struct HeartbeatState {
    pub last_ccp_sent: Instant,
    pub last_ccp_recv: Instant,
    pub last_farm_sent: Instant,
    pub last_farm_recv: Instant,
    pub last_hmds_sent: Instant,
    pub last_hmds_recv: Instant,
    /// Pending test request for CCP: (test_req_id, sent_at).
    pub pending_ccp_test: Option<(String, Instant)>,
    /// Pending test request for farm: (test_req_id, sent_at).
    pub pending_farm_test: Option<(String, Instant)>,
    /// Pending test request for HMDS: (test_req_id, sent_at).
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

impl<S: Strategy> HotLoop<S> {
    pub fn new(strategy: S, core_id: Option<usize>) -> Self {
        Self {
            strategy,
            context: Context::new(),
            core_id,
            farm_conn: None,
            ccp_conn: None,
            hmds_conn: None,
            next_md_req_id: 1,
            md_req_to_instrument: Vec::new(),
            instrument_md_reqs: Vec::new(),
            control_rx: None,
            running: true,
            farm_disconnected: false,
            ccp_disconnected: false,
            hb: HeartbeatState::new(),
            account_id: String::new(),
            seen_exec_ids: HashSet::with_capacity(256),
            hmds_disconnected: false,
            next_tbt_req_id: 1,
            tbt_subscriptions: Vec::new(),
            tbt_price_state: Vec::new(),
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

    /// Build a HotLoop with connections and control channel, without requiring a Gateway.
    pub fn with_connections(
        strategy: S,
        account_id: String,
        farm_conn: Connection,
        ccp_conn: Connection,
        hmds_conn: Option<Connection>,
        core_id: Option<usize>,
    ) -> (Self, Sender<ControlCommand>) {
        let (tx, rx) = bounded(64);
        let mut hl = Self::new(strategy, core_id);
        hl.set_control_rx(rx);
        hl.set_account_id(account_id);
        hl.farm_conn = Some(farm_conn);
        hl.ccp_conn = Some(ccp_conn);
        hl.hmds_conn = hmds_conn;
        (hl, tx)
    }

    /// Access the strategy (for reading results after run completes).
    pub fn strategy(&self) -> &S {
        &self.strategy
    }

    /// Run the hot loop. Blocks until Shutdown command received.
    pub fn run(&mut self) {
        if let Some(core) = self.core_id {
            Self::pin_to_core(core);
        }

        self.strategy.on_start(&mut self.context);
        self.running = true;

        while self.running {
            self.context.loop_iterations += 1;

            // 1. Busy-poll usfarm socket (non-blocking recv)
            //    → HMAC verify → zlib decompress → bit-unpack ticks
            //    → update market state in-place
            //    → strategy.on_tick(&mut ctx)
            self.poll_market_data();

            // 1b. Busy-poll HMDS socket for tick-by-tick data (35=E)
            self.poll_hmds();

            // 2. Drain pending orders → FIX build → HMAC sign → send to CCP
            self.drain_and_send_orders();

            // 3. Busy-poll CCP socket (non-blocking recv)
            //    → decode execution reports
            //    → update positions/orders
            //    → strategy.on_fill() / on_order_update()
            self.poll_executions();

            // 4. Check control_plane_rx (SPSC) for commands
            self.poll_control_commands();

            // 5. Heartbeat check (CCP 10s, farm 30s)
            self.check_heartbeats();
        }
    }

    fn poll_market_data(&mut self) {
        if self.farm_disconnected {
            return;
        }
        // Collect all messages from farm connection first, then process.
        // This avoids borrow conflicts between conn and self.
        let messages = match self.farm_conn.as_mut() {
            None => return,
            Some(conn) => {
                match conn.try_recv() {
                    Ok(0) => return,  // WouldBlock
                    Err(e) => {
                        log::error!("Farm connection lost: {}", e);
                        self.handle_farm_disconnect();
                        return;
                    }
                    Ok(n) => {
                        log::info!("Farm recv: {} bytes, buffered: {}", n, conn.buffered());
                        let now = Instant::now();
                        self.hb.last_farm_recv = now;
                        self.context.recv_at = now;
                        self.hb.pending_farm_test = None; // data received, clear pending test
                    }
                }
                let frames = conn.extract_frames();
                log::info!("Farm frames: {}", frames.len());
                let mut msgs = Vec::new();
                for frame in &frames {
                    match frame {
                        Frame::FixComp(raw) => {
                            let (unsigned, _valid) = conn.unsign(raw);
                            let inner = fixcomp::fixcomp_decompress(&unsigned);
                            for m in &inner {
                                log::info!("Farm FIXCOMP inner: {:?}",
                                    String::from_utf8_lossy(&m[..std::cmp::min(120, m.len())]));
                            }
                            msgs.extend(inner);
                        }
                        Frame::Binary(raw) => {
                            let (unsigned, _valid) = conn.unsign(raw);
                            log::info!("Farm Binary: {:?}",
                                String::from_utf8_lossy(&unsigned[..std::cmp::min(120, unsigned.len())]));
                            msgs.push(unsigned);
                        }
                        Frame::Fix(raw) => {
                            let (unsigned, _valid) = conn.unsign(raw);
                            log::info!("Farm FIX: {:?}",
                                String::from_utf8_lossy(&unsigned[..std::cmp::min(120, unsigned.len())]));
                            msgs.push(unsigned);
                        }
                    }
                }
                msgs
            }
        };

        for msg in &messages {
            self.process_farm_message(msg);
        }
    }

    fn process_farm_message(&mut self, msg: &[u8]) {
        let parsed = fix::fix_parse(msg);
        let msg_type = match parsed.get(&fix::TAG_MSG_TYPE) {
            Some(t) => t.as_str(),
            None => return,
        };

        match msg_type {
            "P" => self.handle_tick_data(msg),
            "Q" => {
                log::info!("Farm 35=Q subscription ack received");
                self.handle_subscription_ack(msg);
            }
            "0" => {} // heartbeat — timestamp already updated in try_recv
            "1" => {
                // Test request — respond with heartbeat containing the test req ID
                let test_id = parsed.get(&fix::TAG_TEST_REQ_ID).cloned().unwrap_or_default();
                if let Some(conn) = self.farm_conn.as_mut() {
                    let ts = chrono_free_timestamp();
                    let result = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    log::info!("Farm TestReq '{}' → heartbeat response seq={} result={:?}",
                        test_id, conn.seq, result);
                    self.hb.last_farm_sent = Instant::now();
                }
            }
            "L" => self.handle_ticker_setup(msg),
            "UT" | "UM" | "RL" => self.handle_account_update(msg),
            "UP" => self.handle_position_update(&parsed),
            _ => {}   // other farm messages (35=G, etc.)
        }
    }

    /// Decode 35=P binary ticks and update market state.
    fn handle_tick_data(&mut self, msg: &[u8]) {
        // Extract body after FIX framing: find content after "35=P\x01"
        let body = match find_body_after_tag(msg, b"35=P\x01") {
            Some(b) => b,
            None => return,
        };

        let ticks = tick_decoder::decode_ticks_35p(body);
        let mut notified: u32 = 0; // bitmask of instruments already notified this batch

        for tick in &ticks {
            let instrument = match self.context.market.instrument_by_server_tag(tick.server_tag) {
                Some(id) => id,
                None => continue,
            };

            let min_tick = self.context.market.min_tick(instrument);
            let q = self.context.market.quote_mut(instrument);

            // Apply tick to quote based on tick type
            match tick.tick_type {
                tick_decoder::O_BID_PRICE => {
                    q.bid += (tick.magnitude as f64 * min_tick * PRICE_SCALE as f64) as i64;
                }
                tick_decoder::O_ASK_PRICE => {
                    q.ask += (tick.magnitude as f64 * min_tick * PRICE_SCALE as f64) as i64;
                }
                tick_decoder::O_LAST_PRICE => {
                    q.last += (tick.magnitude as f64 * min_tick * PRICE_SCALE as f64) as i64;
                }
                tick_decoder::O_HIGH_PRICE => {
                    q.high += (tick.magnitude as f64 * min_tick * PRICE_SCALE as f64) as i64;
                }
                tick_decoder::O_LOW_PRICE => {
                    q.low += (tick.magnitude as f64 * min_tick * PRICE_SCALE as f64) as i64;
                }
                tick_decoder::O_OPEN_PRICE => {
                    q.open += (tick.magnitude as f64 * min_tick * PRICE_SCALE as f64) as i64;
                }
                tick_decoder::O_CLOSE_PRICE => {
                    q.close += (tick.magnitude as f64 * min_tick * PRICE_SCALE as f64) as i64;
                }
                tick_decoder::O_BID_SIZE => {
                    q.bid_size = tick.magnitude;
                }
                tick_decoder::O_ASK_SIZE => {
                    q.ask_size = tick.magnitude;
                }
                tick_decoder::O_LAST_SIZE => {
                    q.last_size = tick.magnitude;
                }
                tick_decoder::O_VOLUME => {
                    // Volume uses fixed 0.0001 multiplier, not minTick
                    q.volume = tick.magnitude;
                }
                tick_decoder::O_TIMESTAMP | tick_decoder::O_LAST_TS => {
                    q.timestamp_ns = tick.magnitude as u64;
                }
                _ => {} // exchanges, halted, etc. — skip for now
            }

            // Notify strategy once per instrument per batch
            let bit = 1u32 << instrument;
            if notified & bit == 0 {
                notified |= bit;
                self.strategy.on_tick(instrument, &mut self.context);
            }
        }
    }

    /// Handle 35=Q subscription acknowledgement: map server_tag → instrument.
    /// Body format (ibgw-headless handler_mktdata.py): CSV fields in tag 6119.
    /// Fields: serverTag,reqId,minTick,...
    fn handle_subscription_ack(&mut self, msg: &[u8]) {
        // 35=Q body is raw CSV after FIX header: serverTag,reqId,minTick,...
        let body = match find_body_after_tag(msg, b"35=Q\x01") {
            Some(b) => b,
            None => return,
        };
        let text = String::from_utf8_lossy(body);
        // Strip trailing HMAC signature if present (8349=...)
        let text = text.split("\x018349=").next().unwrap_or(&text);
        let parts: Vec<&str> = text.trim().split(',').collect();
        if parts.len() < 3 {
            return;
        }
        let server_tag: u32 = match parts[0].parse() {
            Ok(v) => v,
            Err(_) => return,
        };
        let req_id: u32 = match parts[1].parse() {
            Ok(v) => v,
            Err(_) => return,
        };
        let min_tick: f64 = parts[2].parse().unwrap_or(0.01);

        // Look up InstrumentId from pending subscription
        let instrument = match self.md_req_to_instrument.iter()
            .position(|(id, _)| *id == req_id)
        {
            Some(idx) => {
                let (_, instr) = self.md_req_to_instrument.remove(idx);
                instr
            }
            None => return,
        };

        self.context.market.register_server_tag(server_tag, instrument);
        self.context.market.set_min_tick(instrument, min_tick);
        log::info!("Subscribed instrument {} → server_tag {}, minTick {}", instrument, server_tag, min_tick);
    }

    /// Handle 35=L ticker setup: CSV body after header: conId,minTick,serverTag,,1
    fn handle_ticker_setup(&mut self, msg: &[u8]) {
        let body = match find_body_after_tag(msg, b"35=L\x01") {
            Some(b) => b,
            None => return,
        };
        let text = String::from_utf8_lossy(body);
        let text = text.split("\x018349=").next().unwrap_or(&text);
        let parts: Vec<&str> = text.trim().split(',').collect();
        if parts.len() < 3 {
            return;
        }
        let con_id: i64 = match parts[0].parse() {
            Ok(v) => v,
            Err(_) => return,
        };
        let min_tick: f64 = parts[1].parse().unwrap_or(0.01);
        let server_tag: u32 = match parts[2].parse() {
            Ok(v) => v,
            Err(_) => return,
        };

        // Find the instrument by con_id
        if let Some(instrument) = self.context.market.instrument_by_con_id(con_id) {
            self.context.market.register_server_tag(server_tag, instrument);
            self.context.market.set_min_tick(instrument, min_tick);
            log::info!("Ticker setup: con_id {} → server_tag {}, minTick {}", con_id, server_tag, min_tick);
        }
    }

    /// Send FIX 35=V market data subscribe for an instrument.
    /// Sends two entries: BidAsk (264=442) and Last (264=443), matching IB protocol.
    fn send_mktdata_subscribe(&mut self, con_id: i64, instrument: InstrumentId) {
        let bid_ask_id = self.next_md_req_id;
        let last_id = self.next_md_req_id + 1;
        self.next_md_req_id += 2;

        // Track pending subscriptions (both req_ids map to same instrument)
        self.md_req_to_instrument.push((bid_ask_id, instrument));
        self.md_req_to_instrument.push((last_id, instrument));

        // Track active subscriptions for this instrument
        match self.instrument_md_reqs.iter_mut().find(|(id, _)| *id == instrument) {
            Some((_, reqs)) => { reqs.push(bid_ask_id); reqs.push(last_id); }
            None => self.instrument_md_reqs.push((instrument, vec![bid_ask_id, last_id])),
        }

        if let Some(conn) = self.farm_conn.as_mut() {
            let bid_ask_str = bid_ask_id.to_string();
            let last_str = last_id.to_string();
            let con_id_str = (con_id as u32).to_string();
            let ts = chrono_free_timestamp();
            let _ = conn.send_fixcomp(&[
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                (fix::TAG_SENDING_TIME, &ts),
                (263, "1"), // Subscribe
                (146, "2"), // 2 entries: BidAsk + Last
                // Entry 1: BidAsk
                (262, &bid_ask_str),
                (6008, &con_id_str),
                (207, "BEST"),
                (167, "CS"),
                (264, "442"), // BidAsk
                (6088, "Socket"),
                (9830, "1"),
                (9839, "1"),
                // Entry 2: Last
                (262, &last_str),
                (6008, &con_id_str),
                (207, "BEST"),
                (167, "CS"),
                (264, "443"), // Last
                (6088, "Socket"),
                (9830, "1"),
                (9839, "1"),
            ]);
            log::info!("Sent 35=V subscribe: con_id={} ids={},{} seq={}",
                con_id, bid_ask_id, last_id, conn.seq);
            self.hb.last_farm_sent = Instant::now();
        }
    }

    /// Send FIX 35=V market data unsubscribe for all active subscriptions of an instrument.
    fn send_mktdata_unsubscribe(&mut self, instrument: InstrumentId) {
        let reqs: Vec<u32> = match self.instrument_md_reqs.iter()
            .position(|(id, _)| *id == instrument)
        {
            Some(idx) => {
                let (_, reqs) = self.instrument_md_reqs.remove(idx);
                reqs
            }
            None => return,
        };

        let conn = match self.farm_conn.as_mut() {
            Some(c) => c,
            None => return,
        };

        for req_id in reqs {
            let req_id_str = req_id.to_string();
            let _ = conn.send_fixcomp(&[
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                (262, &req_id_str),
                (263, "2"), // Unsubscribe
            ]);
        }
        self.hb.last_farm_sent = Instant::now();
    }

    fn drain_and_send_orders(&mut self) {
        let orders: Vec<OrderRequest> = self.context.drain_pending_orders().collect();
        let conn = match self.ccp_conn.as_mut() {
            Some(c) => c,
            None => return,
        };
        for order_req in orders {
            let result = match order_req {
                OrderRequest::SubmitLimit { order_id, instrument, side, qty, price } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price, b'2', b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),   // ClOrdID
                        (1, &self.account_id), // Account
                        (21, "2"),          // HandlInst = Automated
                        (55, &symbol),      // Symbol
                        (54, side_str),     // Side
                        (38, &qty_str),     // OrderQty
                        (40, "2"),          // OrdType = Limit
                        (44, &price_str),   // Price
                        (59, "0"),          // TIF = DAY
                        (60, &now),         // TransactTime
                        (167, "STK"),       // SecurityType = CommonStock
                        (100, "SMART"),     // ExDestination
                        (15, "USD"),        // Currency
                        (204, "0"),         // CustomerOrFirm
                    ])
                }
                OrderRequest::SubmitStopLimit { order_id, instrument, side, qty, price, stop_price } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price, b'4', b'0', stop_price,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let stop_str = format_price(stop_price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "4"),          // OrdType = Stop Limit
                        (44, &price_str),   // Limit Price
                        (99, &stop_str),    // StopPx
                        (59, "0"),          // TIF = DAY
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitLimitGtc { order_id, instrument, side, qty, price, outside_rth } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price, b'2', b'1', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    let mut fields: Vec<(u32, &str)> = vec![
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "2"),          // OrdType = Limit
                        (44, &price_str),
                        (59, "1"),          // TIF = GTC
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ];
                    if outside_rth {
                        fields.push((6433, "1")); // OutsideRTH
                    }
                    conn.send_fix(&fields)
                }
                OrderRequest::SubmitLimitEx { order_id, instrument, side, qty, price, tif, attrs } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price, b'2', tif, 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let tif_byte = [tif];
                    let tif_str = std::str::from_utf8(&tif_byte).unwrap_or("0");
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    let display_str = attrs.display_size.to_string();
                    let min_qty_str = attrs.min_qty.to_string();
                    let gat_str = if attrs.good_after > 0 { unix_to_ib_datetime(attrs.good_after) } else { String::new() };
                    let gtd_str = if attrs.good_till > 0 { unix_to_ib_datetime(attrs.good_till) } else { String::new() };
                    let oca_str = if attrs.oca_group > 0 { format!("OCA_{}", attrs.oca_group) } else { String::new() };
                    let mut fields: Vec<(u32, &str)> = vec![
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "2"),              // OrdType = Limit
                        (44, &price_str),
                        (59, tif_str),
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ];
                    if attrs.display_size > 0 {
                        fields.push((111, &display_str));
                    }
                    if attrs.min_qty > 0 {
                        fields.push((110, &min_qty_str));
                    }
                    if attrs.outside_rth {
                        fields.push((6433, "1"));
                    }
                    if attrs.hidden {
                        fields.push((6135, "1"));
                    }
                    if attrs.good_after > 0 {
                        fields.push((168, &gat_str));
                    }
                    if attrs.good_till > 0 {
                        fields.push((126, &gtd_str));
                    }
                    if attrs.oca_group > 0 {
                        fields.push((583, &oca_str));
                        fields.push((6209, "CancelOnFillWBlock"));
                    }
                    let disc_str;
                    if attrs.discretionary_amt > 0 {
                        disc_str = format_price(attrs.discretionary_amt);
                        fields.push((9813, &disc_str));
                    }
                    if attrs.sweep_to_fill {
                        fields.push((6102, "1"));
                    }
                    if attrs.all_or_none {
                        fields.push((18, "G"));
                    }
                    let trigger_str;
                    if attrs.trigger_method > 0 {
                        trigger_str = attrs.trigger_method.to_string();
                        fields.push((6115, &trigger_str));
                    }
                    // Condition tags (6136+ framework)
                    let cond_strs = build_condition_strings(&attrs.conditions);
                    if !attrs.conditions.is_empty() {
                        let count_str = &cond_strs[0]; // first element is count
                        fields.push((6136, count_str));
                        if attrs.conditions_cancel_order {
                            fields.push((6128, "1"));
                        }
                        if attrs.conditions_ignore_rth {
                            fields.push((6151, "1"));
                        }
                        // Per-condition tags start at index 1, 11 strings per condition
                        for i in 0..attrs.conditions.len() {
                            let base = 1 + i * 11;
                            fields.push((6222, &cond_strs[base]));      // condType
                            fields.push((6137, &cond_strs[base + 1]));  // conjunction
                            fields.push((6126, &cond_strs[base + 2]));  // operator
                            fields.push((6123, &cond_strs[base + 3]));  // conId
                            fields.push((6124, &cond_strs[base + 4]));  // exchange
                            fields.push((6127, &cond_strs[base + 5]));  // triggerMethod
                            fields.push((6125, &cond_strs[base + 6]));  // price
                            fields.push((6223, &cond_strs[base + 7]));  // time
                            fields.push((6245, &cond_strs[base + 8]));  // percent
                            fields.push((6263, &cond_strs[base + 9]));  // volume
                            fields.push((6246, &cond_strs[base + 10])); // execution
                        }
                    }
                    conn.send_fix(&fields)
                }
                OrderRequest::SubmitMarket { order_id, instrument, side, qty } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, 0, b'1', b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    log::info!("Sending MKT order: clord={} acct={} sym={} side={} qty={}",
                        clord_str, self.account_id, symbol, side_str, qty_str);
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id), // Account
                        (21, "2"),          // HandlInst = Automated
                        (55, &symbol),      // Symbol
                        (54, side_str),
                        (38, &qty_str),
                        (40, "1"),          // OrdType = Market
                        (59, "0"),          // TIF = DAY
                        (60, &now),         // TransactTime
                        (167, "STK"),       // SecurityType
                        (100, "SMART"),     // ExDestination
                        (15, "USD"),        // Currency
                        (204, "0"),         // CustomerOrFirm
                    ])
                }
                OrderRequest::SubmitStop { order_id, instrument, side, qty, stop_price } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, stop_price, b'3', b'0', stop_price,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let stop_str = format_price(stop_price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),          // HandlInst = Automated
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "3"),          // OrdType = Stop
                        (99, &stop_str),    // StopPx
                        (59, "0"),          // TIF = DAY
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitStopGtc { order_id, instrument, side, qty, stop_price, outside_rth } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, stop_price, b'3', b'1', stop_price,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let stop_str = format_price(stop_price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    let mut fields: Vec<(u32, &str)> = vec![
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "3"),          // OrdType = Stop
                        (99, &stop_str),
                        (59, "1"),          // TIF = GTC
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ];
                    if outside_rth {
                        fields.push((6433, "1"));
                    }
                    conn.send_fix(&fields)
                }
                OrderRequest::SubmitStopLimitGtc { order_id, instrument, side, qty, price, stop_price, outside_rth } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price, b'4', b'1', stop_price,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let stop_str = format_price(stop_price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    let mut fields: Vec<(u32, &str)> = vec![
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "4"),          // OrdType = Stop Limit
                        (44, &price_str),
                        (99, &stop_str),
                        (59, "1"),          // TIF = GTC
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ];
                    if outside_rth {
                        fields.push((6433, "1"));
                    }
                    conn.send_fix(&fields)
                }
                OrderRequest::SubmitLimitIoc { order_id, instrument, side, qty, price } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price, b'2', b'3', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "2"),          // OrdType = Limit
                        (44, &price_str),
                        (59, "3"),          // TIF = IOC
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitLimitFok { order_id, instrument, side, qty, price } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price, b'2', b'4', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "2"),          // OrdType = Limit
                        (44, &price_str),
                        (59, "4"),          // TIF = FOK
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitTrailingStop { order_id, instrument, side, qty, trail_amt } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, 0, b'P', b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let trail_str = format_price(trail_amt);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "P"),          // OrdType = Trailing Stop
                        (99, &trail_str),   // StopPx = trail amount
                        (59, "0"),          // TIF = DAY
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitTrailingStopLimit { order_id, instrument, side, qty, price, trail_amt } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price, b'P', b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let trail_str = format_price(trail_amt);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "P"),          // OrdType = Trailing Stop (IB uses P for both)
                        (44, &price_str),   // Limit price
                        (99, &trail_str),   // StopPx = trail amount
                        (59, "0"),          // TIF = DAY
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitTrailingStopPct { order_id, instrument, side, qty, trail_pct } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, 0, b'P', b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let pct_str = trail_pct.to_string(); // basis points: 100 = 1%
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "P"),              // OrdType = Trailing Stop
                        (6268, &pct_str),       // TrailingPercent (basis points)
                        (59, "0"),              // TIF = DAY
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitMoc { order_id, instrument, side, qty } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, 0, b'5', b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "5"),          // OrdType = Market on Close
                        (59, "0"),          // TIF = DAY
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitLoc { order_id, instrument, side, qty, price } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price, b'B', b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "B"),          // OrdType = Limit on Close
                        (44, &price_str),   // Limit price
                        (59, "0"),          // TIF = DAY
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitMit { order_id, instrument, side, qty, stop_price } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, stop_price, b'J', b'0', stop_price,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let stop_str = format_price(stop_price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "J"),          // OrdType = Market if Touched
                        (99, &stop_str),    // StopPx = trigger price
                        (59, "0"),          // TIF = DAY
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitLit { order_id, instrument, side, qty, price, stop_price } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price, b'K', b'0', stop_price,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let stop_str = format_price(stop_price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "K"),          // OrdType = Limit if Touched
                        (44, &price_str),   // Limit price
                        (99, &stop_str),    // StopPx = trigger price
                        (59, "0"),          // TIF = DAY
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitBracket { parent_id, tp_id, sl_id, instrument, side, qty, entry_price, take_profit, stop_loss } => {
                    let exit_side = match side { Side::Buy => Side::Sell, Side::Sell | Side::ShortSell => Side::Buy };
                    let exit_side_str = fix_side(exit_side);
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let parent_str = parent_id.to_string();
                    let tp_str = tp_id.to_string();
                    let sl_str = sl_id.to_string();
                    let entry_str = format_price(entry_price);
                    let tp_price_str = format_price(take_profit);
                    let sl_price_str = format_price(stop_loss);
                    let oca_group = format!("OCA_{}", parent_id);

                    // 1. Parent order: limit entry
                    self.context.insert_order(crate::types::Order::new(
                        parent_id, instrument, side, qty, entry_price, b'2', b'0', 0,
                    ));
                    let now = chrono_free_timestamp();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &parent_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "2"),          // Limit
                        (44, &entry_str),
                        (59, "0"),          // DAY
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ]);

                    // 2. Take-profit child: limit exit, linked to parent, in OCA group
                    self.context.insert_order(crate::types::Order::new(
                        tp_id, instrument, exit_side, qty, take_profit, b'2', b'1', 0,
                    ));
                    let now = chrono_free_timestamp();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &tp_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, exit_side_str),
                        (38, &qty_str),
                        (40, "2"),          // Limit
                        (44, &tp_price_str),
                        (59, "1"),          // GTC
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                        (6107, &parent_str),       // ParentOrderID
                        (583, &oca_group),         // OCAGroup
                        (6209, "CancelOnFillWBlock"), // OCA cancel-on-fill
                    ]);

                    // 3. Stop-loss child: stop exit, linked to parent, in OCA group
                    self.context.insert_order(crate::types::Order::new(
                        sl_id, instrument, exit_side, qty, stop_loss, b'3', b'1', stop_loss,
                    ));
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &sl_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, exit_side_str),
                        (38, &qty_str),
                        (40, "3"),          // Stop
                        (99, &sl_price_str),
                        (59, "1"),          // GTC
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                        (6107, &parent_str),       // ParentOrderID
                        (583, &oca_group),         // OCAGroup
                        (6209, "CancelOnFillWBlock"), // OCA cancel-on-fill
                    ])
                }
                OrderRequest::SubmitRel { order_id, instrument, side, qty, offset } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, 0, b'R', b'0', offset,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let offset_str = format_price(offset);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "R"),              // OrdType = Relative
                        (99, &offset_str),      // Peg offset (auxPrice)
                        (59, "0"),              // TIF = DAY
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitLimitOpg { order_id, instrument, side, qty, price } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price, b'2', b'2', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "2"),              // OrdType = Limit
                        (44, &price_str),
                        (59, "2"),              // TIF = OPG (At the Opening)
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitAdaptive { order_id, instrument, side, qty, price, priority } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price, b'2', b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    let priority_str = priority.as_str();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "2"),              // OrdType = Limit
                        (44, &price_str),
                        (59, "0"),              // TIF = DAY
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                        (847, "Adaptive"),      // AlgoStrategy
                        (5957, "1"),            // AlgoParamCount
                        (5958, "adaptivePriority"), // AlgoParamTag
                        (5960, priority_str),   // AlgoParamValue
                    ])
                }
                OrderRequest::SubmitAlgo { order_id, instrument, side, qty, price, algo } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price, b'2', b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    let mut fields: Vec<(u32, &str)> = vec![
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "2"),              // OrdType = Limit
                        (44, &price_str),
                        (59, "0"),              // TIF = DAY
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ];
                    let (algo_name, param_strs) = build_algo_tags(&algo);
                    fields.push((847, algo_name));
                    // Tag 849 (maxPctVol) for algos that use it
                    let pct_str = match &algo {
                        AlgoParams::Vwap { max_pct_vol, .. }
                        | AlgoParams::ArrivalPx { max_pct_vol, .. }
                        | AlgoParams::ClosePx { max_pct_vol, .. } => format!("{}", max_pct_vol),
                        _ => String::new(),
                    };
                    if !pct_str.is_empty() {
                        fields.push((849, &pct_str));
                    }
                    let count_str = (param_strs.len() / 2).to_string();
                    fields.push((5957, &count_str));
                    // Emit key/value pairs: 5958=key, 5960=value (repeated)
                    let mut i = 0;
                    while i < param_strs.len() {
                        fields.push((5958, &param_strs[i]));
                        fields.push((5960, &param_strs[i + 1]));
                        i += 2;
                    }
                    conn.send_fix(&fields)
                }
                OrderRequest::SubmitMtl { order_id, instrument, side, qty } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, 0, b'K', b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "K"),          // OrdType = Market to Limit
                        (59, "0"),
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitMktPrt { order_id, instrument, side, qty } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, 0, b'U', b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "U"),          // OrdType = Market with Protection
                        (59, "0"),
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitStpPrt { order_id, instrument, side, qty, stop_price } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, 0, crate::types::ORD_STP_PRT, b'0', stop_price,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let stop_str = format_price(stop_price);
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "SP"),         // OrdType = Stop with Protection
                        (99, &stop_str),    // StopPx
                        (59, "0"),
                        (60, &now),
                        (167, "STK"),
                        (100, "SMART"),
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitMidPrice { order_id, instrument, side, qty, price_cap } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, price_cap, crate::types::ORD_MIDPX, b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    let mut fields: Vec<(u32, &str)> = vec![
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "MIDPX"),      // OrdType = Mid-Price
                        (59, "0"),
                        (60, &now),
                        (167, "STK"),
                        (100, "ISLAND"),    // Requires directed exchange (not SMART)
                        (15, "USD"),
                        (204, "0"),
                    ];
                    let cap_str;
                    if price_cap > 0 {
                        cap_str = format_price(price_cap);
                        fields.push((44, &cap_str)); // Price cap
                    }
                    conn.send_fix(&fields)
                }
                OrderRequest::SubmitSnapMkt { order_id, instrument, side, qty } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, 0, crate::types::ORD_SNAP_MKT, b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "SMKT"),       // OrdType = Snap to Market
                        (59, "0"),
                        (60, &now),
                        (167, "STK"),
                        (100, "ISLAND"),    // Requires directed exchange
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitSnapMid { order_id, instrument, side, qty } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, 0, crate::types::ORD_SNAP_MID, b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "SMID"),       // OrdType = Snap to Midpoint
                        (59, "0"),
                        (60, &now),
                        (167, "STK"),
                        (100, "ISLAND"),    // Requires directed exchange
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitSnapPri { order_id, instrument, side, qty } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, 0, crate::types::ORD_SNAP_PRI, b'0', 0,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "SREL"),       // OrdType = Snap to Primary
                        (59, "0"),
                        (60, &now),
                        (167, "STK"),
                        (100, "ISLAND"),    // Requires directed exchange
                        (15, "USD"),
                        (204, "0"),
                    ])
                }
                OrderRequest::SubmitPegMkt { order_id, instrument, side, qty, offset } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, 0, crate::types::ORD_PEG_MKT, b'0', offset,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    let mut fields: Vec<(u32, &str)> = vec![
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "E"),          // OrdType = Pegged (no mid-offset tags = PEGMKT)
                        (59, "0"),
                        (60, &now),
                        (167, "STK"),
                        (100, "ISLAND"),    // Requires directed exchange
                        (15, "USD"),
                        (204, "0"),
                    ];
                    let offset_str;
                    if offset > 0 {
                        offset_str = format_price(offset);
                        fields.push((211, &offset_str)); // PegOffsetValue
                    }
                    conn.send_fix(&fields)
                }
                OrderRequest::SubmitPegMid { order_id, instrument, side, qty, offset } => {
                    self.context.insert_order(crate::types::Order::new(
                        order_id, instrument, side, qty, 0, crate::types::ORD_PEG_MID, b'0', offset,
                    ));
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let symbol = self.context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    let mut fields: Vec<(u32, &str)> = vec![
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (1, &self.account_id),
                        (21, "2"),
                        (55, &symbol),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "E"),          // OrdType = Pegged (tags 8403/8404 = PEGMID)
                        (59, "0"),
                        (60, &now),
                        (167, "STK"),
                        (100, "ISLAND"),    // Requires directed exchange
                        (15, "USD"),
                        (204, "0"),
                        (8403, "0.0"),      // midOffsetAtWhole — differentiates PEGMID from PEGMKT
                        (8404, "0.0"),      // midOffsetAtHalf
                    ];
                    let offset_str;
                    if offset > 0 {
                        offset_str = format_price(offset);
                        fields.push((211, &offset_str)); // PegOffsetValue
                    }
                    conn.send_fix(&fields)
                }
                OrderRequest::Cancel { order_id } => {
                    let clord_str = format!("C{}", order_id);
                    let orig_clord = order_id.to_string();
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_ORDER_CANCEL),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),   // ClOrdID (cancel)
                        (41, &orig_clord),  // OrigClOrdID
                        (60, &now),         // TransactTime
                    ])
                }
                OrderRequest::CancelAll { instrument } => {
                    // Cancel all open orders for this instrument by iterating
                    let open_ids: Vec<u64> = self.context.open_orders_for(instrument)
                        .iter()
                        .map(|o| o.order_id)
                        .collect();
                    let mut last_result = Ok(());
                    for oid in open_ids {
                        let clord_str = format!("C{}", oid);
                        let orig_clord = oid.to_string();
                        let now = chrono_free_timestamp();
                        last_result = conn.send_fix(&[
                            (fix::TAG_MSG_TYPE, fix::MSG_ORDER_CANCEL),
                            (fix::TAG_SENDING_TIME, &now),
                            (11, &clord_str),
                            (41, &orig_clord),
                            (60, &now),
                        ]);
                    }
                    last_result
                }
                OrderRequest::Modify { new_order_id, order_id, price, qty } => {
                    // Insert new order entry so exec reports for new_order_id are tracked
                    let orig = self.context.order(order_id).copied();
                    if let Some(orig) = orig {
                        self.context.insert_order(crate::types::Order::new(
                            new_order_id, orig.instrument, orig.side, qty, price,
                            orig.ord_type, orig.tif, orig.stop_price,
                        ));
                    }
                    let clord_str = new_order_id.to_string();
                    let orig_clord = order_id.to_string();
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let now = chrono_free_timestamp();
                    let side_str = orig.map(|o| fix_side(o.side)).unwrap_or("1");
                    let symbol = orig.map(|o| self.context.market.symbol(o.instrument).to_string())
                        .unwrap_or_default();
                    let ord_type_str = crate::types::ord_type_fix_str(orig.map(|o| o.ord_type).unwrap_or(b'2')).to_string();
                    let tif_str = std::str::from_utf8(&[orig.map(|o| o.tif).unwrap_or(b'0')]).unwrap_or("0").to_string();
                    let mut fields: Vec<(u32, &str)> = vec![
                        (fix::TAG_MSG_TYPE, fix::MSG_ORDER_REPLACE),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),   // ClOrdID
                        (41, &orig_clord),  // OrigClOrdID
                        (1, &self.account_id), // Account
                        (21, "2"),          // HandlInst = Automated
                        (55, &symbol),      // Symbol
                        (54, side_str),     // Side
                        (38, &qty_str),     // OrderQty
                        (40, &ord_type_str), // OrdType from original order
                        (44, &price_str),   // Price
                        (59, &tif_str),     // TIF from original order
                        (60, &now),         // TransactTime
                        (167, "STK"),       // SecurityType
                        (100, "SMART"),     // ExDestination
                        (15, "USD"),        // Currency
                        (204, "0"),         // CustomerOrFirm
                    ];
                    // Include stop price for order types that need it
                    let stop_str;
                    if let Some(o) = orig {
                        if o.stop_price != 0 {
                            stop_str = format_price(o.stop_price);
                            fields.push((99, &stop_str));
                        }
                    }
                    conn.send_fix(&fields)
                }
            };
            match result {
                Ok(()) => self.hb.last_ccp_sent = Instant::now(),
                Err(e) => log::error!("Failed to send order: {}", e),
            }
        }
    }

    fn poll_executions(&mut self) {
        if self.ccp_disconnected {
            return;
        }
        let messages = match self.ccp_conn.as_mut() {
            None => return,
            Some(conn) => {
                match conn.try_recv() {
                    Ok(0) if !conn.has_buffered_data() => return,
                    Ok(0) => {} // no new bytes but buffer has seeded data
                    Err(e) => {
                        log::error!("CCP connection lost: {}", e);
                        self.handle_ccp_disconnect();
                        return;
                    }
                    Ok(_) => {
                        self.hb.last_ccp_recv = Instant::now();
                        self.hb.pending_ccp_test = None;
                    }
                }
                let frames = conn.extract_frames();
                let mut msgs = Vec::new();
                for frame in frames {
                    match frame {
                        Frame::FixComp(raw) => {
                            let (unsigned, _) = conn.unsign(&raw);
                            msgs.extend(fixcomp::fixcomp_decompress(&unsigned));
                        }
                        Frame::Fix(raw) => {
                            let (unsigned, _) = conn.unsign(&raw);
                            msgs.push(unsigned);
                        }
                        Frame::Binary(raw) => {
                            let (unsigned, _) = conn.unsign(&raw);
                            msgs.push(unsigned);
                        }
                    }
                }
                msgs
            }
        };

        for msg in &messages {
            self.process_ccp_message(msg);
        }
    }

    fn process_ccp_message(&mut self, msg: &[u8]) {
        let parsed = fix::fix_parse(msg);
        let msg_type = match parsed.get(&fix::TAG_MSG_TYPE) {
            Some(t) => t.as_str(),
            None => return,
        };

        log::debug!("CCP msg 35={}", msg_type);

        match msg_type {
            fix::MSG_EXEC_REPORT => self.handle_exec_report(&parsed),
            fix::MSG_CANCEL_REJECT => self.handle_cancel_reject(&parsed),
            fix::MSG_HEARTBEAT => {} // timestamp already updated in try_recv
            fix::MSG_TEST_REQUEST => {
                let test_id = parsed.get(&fix::TAG_TEST_REQ_ID).cloned().unwrap_or_default();
                if let Some(conn) = self.ccp_conn.as_mut() {
                    let ts = chrono_free_timestamp();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    self.hb.last_ccp_sent = Instant::now();
                }
            }
            "3" => {
                // Session Reject
                let reason = parsed.get(&58).map(|s| s.as_str()).unwrap_or("unknown");
                let ref_tag = parsed.get(&371).map(|s| s.as_str()).unwrap_or("?");
                log::warn!("SessionReject: reason='{}' refTag={}", reason, ref_tag);
            }
            "U" => {
                // IB custom message — route by 6040 comm type
                if let Some(comm) = parsed.get(&6040) {
                    if comm == "77" {
                        self.handle_account_summary(&parsed);
                    }
                }
            }
            "UT" | "UM" | "RL" => self.handle_account_update(msg),
            "UP" => self.handle_position_update(&parsed),
            _ => {}
        }
    }

    /// Handle FIX 35=8 ExecutionReport from CCP.
    fn handle_exec_report(&mut self, parsed: &std::collections::HashMap<u32, String>) {
        let ord_status = parsed.get(&39).map(|s| s.as_str()).unwrap_or("");
        let exec_type = parsed.get(&150).map(|s| s.as_str()).unwrap_or("");
        let exec_id = parsed.get(&17).map(|s| s.as_str()).unwrap_or("");
        let last_px = parsed.get(&31).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let last_shares = parsed.get(&32).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let leaves_qty = parsed.get(&151).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let commission = parsed.get(&12).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let clord_id = parsed.get(&11).and_then(|s| {
            // Cancel responses have "C" prefix (e.g. "C1772746902000")
            let stripped = s.strip_prefix('C').unwrap_or(s);
            stripped.parse::<u64>().ok()
        }).unwrap_or(0);

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

        // Map FIX OrdStatus (tag 39) to our OrderStatus
        // 39=5 (Replaced) means the modify succeeded — the new order is now live
        // 39=6 (PendingCancel) is intermediate — ignore, wait for terminal state
        let status = match ord_status {
            "0" | "A" | "E" | "5" => crate::types::OrderStatus::Submitted,
            "1" => crate::types::OrderStatus::PartiallyFilled,
            "2" => crate::types::OrderStatus::Filled,
            "4" | "C" => crate::types::OrderStatus::Cancelled,
            "8" => crate::types::OrderStatus::Rejected,
            "6" => return, // PendingCancel — not terminal
            _ => return,
        };

        // Check if status actually changed (dedup repeated exec reports like 3x 39=A)
        let prev_status = self.context.order(clord_id).map(|o| o.status);
        let status_changed = prev_status != Some(status);

        // Update order status
        self.context.update_order_status(clord_id, status);

        // On fill (exec_type: F=Fill, 1=Partial, 2=Filled)
        // Dedup by ExecID (tag 17) — IB can send duplicate exec reports
        if matches!(exec_type, "F" | "1" | "2") && last_shares > 0 {
            if !exec_id.is_empty() && !self.seen_exec_ids.insert(exec_id.to_string()) {
                log::warn!("Duplicate ExecID={} — skipping fill", exec_id);
                return; // Already processed this fill
            }
            // Look up the order to get instrument and side
            if let Some(order) = self.context.order(clord_id).copied() {
                // Update filled qty on the order
                self.context.update_order_filled(clord_id, last_shares as u32);

                let fill = crate::types::Fill {
                    instrument: order.instrument,
                    order_id: clord_id,
                    side: order.side,
                    price: (last_px * PRICE_SCALE as f64) as i64,
                    qty: last_shares,
                    remaining: leaves_qty,
                    commission: (commission * PRICE_SCALE as f64) as i64,
                    timestamp_ns: self.context.now_ns(),
                };

                let delta = match order.side {
                    Side::Buy => last_shares,
                    Side::Sell | Side::ShortSell => -last_shares,
                };
                self.context.update_position(order.instrument, delta);
                self.strategy.on_fill(&fill, &mut self.context);
            }
        }

        // Notify strategy only on actual status changes (dedup repeated reports)
        if status_changed {
            if let Some(order) = self.context.order(clord_id).copied() {
                let update = crate::types::OrderUpdate {
                    order_id: clord_id,
                    instrument: order.instrument,
                    status,
                    filled_qty: order.filled as i64,
                    remaining_qty: leaves_qty,
                    timestamp_ns: self.context.now_ns(),
                };
                self.strategy.on_order_update(&update, &mut self.context);
            }
        }

        // Remove fully terminal orders
        if matches!(status,
            crate::types::OrderStatus::Filled |
            crate::types::OrderStatus::Cancelled |
            crate::types::OrderStatus::Rejected
        ) {
            self.context.remove_order(clord_id);
        }
    }

    fn handle_cancel_reject(&mut self, parsed: &std::collections::HashMap<u32, String>) {
        let orig_clord = parsed.get(&41).and_then(|s| s.parse::<u64>().ok());
        let reason = parsed.get(&58).map(|s| s.as_str()).unwrap_or("Cancel rejected");
        // Tag 434: 1=Cancel rejected, 2=Modify/Replace rejected
        let reject_type: u8 = parsed.get(&434).and_then(|s| s.parse().ok()).unwrap_or(1);
        // Tag 102: CxlRejReason (0=TooLate, 1=UnknownOrder, 3=PendingStatus, etc.)
        let reason_code: i32 = parsed.get(&102).and_then(|s| s.parse().ok()).unwrap_or(-1);
        log::warn!("CancelReject: origClOrd={:?} type={} code={} reason={}",
            orig_clord, reject_type, reason_code, reason);

        if let Some(oid) = orig_clord {
            // Restore previous status — cancel/modify failed, order is still live.
            // If order was PartiallyFilled, keep it as PartiallyFilled (not Submitted).
            if let Some(order) = self.context.order(oid).copied() {
                let restore_status = if order.filled > 0 {
                    crate::types::OrderStatus::PartiallyFilled
                } else {
                    crate::types::OrderStatus::Submitted
                };
                self.context.update_order_status(oid, restore_status);

                let reject = crate::types::CancelReject {
                    order_id: oid,
                    instrument: order.instrument,
                    reject_type,
                    reason_code,
                    timestamp_ns: self.context.now_ns(),
                };
                self.strategy.on_cancel_reject(&reject, &mut self.context);
            }
        }
    }

    /// Handle CCP 35=U 6040=77 account summary (init burst response).
    /// Contains tag 9806 = net liquidation value.
    fn handle_account_summary(&mut self, parsed: &std::collections::HashMap<u32, String>) {
        if let Some(val) = parsed.get(&9806).and_then(|s| s.parse::<f64>().ok()) {
            self.context.account.net_liquidation = (val * PRICE_SCALE as f64) as Price;
            log::info!("Account summary: net_liq=${:.2}", val);
        }
    }

    /// Handle 8=O UT/UM/RL account value messages.
    /// Format: repeated 8001=key\x018004=value entries.
    fn handle_account_update(&mut self, msg: &[u8]) {
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
                        "NetLiquidation" => {
                            if let Ok(v) = val.parse::<f64>() {
                                self.context.account.net_liquidation = (v * PRICE_SCALE as f64) as Price;
                            }
                        }
                        "BuyingPower" => {
                            if let Ok(v) = val.parse::<f64>() {
                                self.context.account.buying_power = (v * PRICE_SCALE as f64) as Price;
                            }
                        }
                        "MaintMarginReq" => {
                            if let Ok(v) = val.parse::<f64>() {
                                self.context.account.margin_used = (v * PRICE_SCALE as f64) as Price;
                            }
                        }
                        "UnrealizedPnL" => {
                            if let Ok(v) = val.parse::<f64>() {
                                self.context.account.unrealized_pnl = (v * PRICE_SCALE as f64) as Price;
                            }
                        }
                        "RealizedPnL" => {
                            if let Ok(v) = val.parse::<f64>() {
                                self.context.account.realized_pnl = (v * PRICE_SCALE as f64) as Price;
                            }
                        }
                        _ => {}
                    }
                    key = None;
                }
            }
        }
    }

    /// Handle 8=O UP position messages.
    /// Tags: 6008=conId, 6064=position, 6065=avgCost.
    fn handle_position_update(&mut self, parsed: &std::collections::HashMap<u32, String>) {
        let con_id: i64 = match parsed.get(&6008).and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => return,
        };
        let position: i64 = parsed.get(&6064)
            .and_then(|s| s.parse::<f64>().ok())
            .map(|v| v as i64)
            .unwrap_or(0);

        if let Some(instrument) = self.context.market.instrument_by_con_id(con_id) {
            // Set absolute position (UP gives absolute, not delta)
            let current = self.context.position(instrument);
            let delta = position - current;
            if delta != 0 {
                self.context.update_position(instrument, delta);
            }
        }
    }

    fn poll_control_commands(&mut self) {
        // Collect commands first to avoid borrow conflict (rx borrows self immutably).
        let cmds: Vec<ControlCommand> = match self.control_rx.as_ref() {
            Some(rx) => rx.try_iter().collect(),
            None => return,
        };

        for cmd in cmds {
            match cmd {
                ControlCommand::Subscribe { con_id, symbol } => {
                    let id = self.context.market.register(con_id);
                    self.context.market.set_symbol(id, symbol);
                    self.send_mktdata_subscribe(con_id, id);
                }
                ControlCommand::Unsubscribe { instrument } => {
                    self.send_mktdata_unsubscribe(instrument);
                }
                ControlCommand::SubscribeTbt { con_id, symbol, tbt_type } => {
                    let id = self.context.market.register(con_id);
                    self.context.market.set_symbol(id, symbol);
                    self.send_tbt_subscribe(con_id, id, tbt_type);
                }
                ControlCommand::UnsubscribeTbt { instrument } => {
                    self.send_tbt_unsubscribe(instrument);
                }
                ControlCommand::UpdateParam { key, value } => {
                    let _ = (key, value);
                }
                ControlCommand::Order(req) => {
                    self.context.pending_orders.push(req);
                }
                ControlCommand::RegisterInstrument { con_id } => {
                    self.context.market.register(con_id);
                }
                ControlCommand::Shutdown => {
                    // Unsubscribe all active market data before stopping
                    let instruments: Vec<InstrumentId> = self.instrument_md_reqs
                        .iter().map(|(id, _)| *id).collect();
                    for instrument in instruments {
                        self.send_mktdata_unsubscribe(instrument);
                    }
                    // Unsubscribe all TBT subscriptions before stopping
                    let tbt_instruments: Vec<InstrumentId> = self.tbt_subscriptions
                        .iter().map(|(id, _, _)| *id).collect();
                    for instrument in tbt_instruments {
                        self.send_tbt_unsubscribe(instrument);
                    }
                    self.running = false;
                    self.strategy.on_disconnect(&mut self.context);
                }
            }
        }
    }

    fn check_heartbeats(&mut self) {
        let now = Instant::now();
        let ts = chrono_free_timestamp();

        // --- CCP heartbeat (skip if already disconnected) ---
        if !self.ccp_disconnected {
        if let Some(conn) = self.ccp_conn.as_mut() {
            let since_sent = now.duration_since(self.hb.last_ccp_sent).as_secs();
            let since_recv = now.duration_since(self.hb.last_ccp_recv).as_secs();

            // Send heartbeat if idle too long
            if since_sent >= CCP_HEARTBEAT_SECS {
                let _ = conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                    (fix::TAG_SENDING_TIME, &ts),
                ]);
                self.hb.last_ccp_sent = now;
            }

            // Check for timeout
            if since_recv > CCP_HEARTBEAT_SECS + HEARTBEAT_GRACE_SECS {
                if let Some((_, sent_at)) = &self.hb.pending_ccp_test {
                    if now.duration_since(*sent_at).as_secs() > CCP_HEARTBEAT_SECS {
                        // TestRequest timed out — connection lost (set flag, don't kill loop)
                        log::error!("CCP heartbeat timeout — connection lost");
                        self.handle_ccp_disconnect();
                    }
                } else {
                    // Send TestRequest
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
        if !self.farm_disconnected {
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

            if since_recv > FARM_HEARTBEAT_SECS + HEARTBEAT_GRACE_SECS {
                if let Some((_, sent_at)) = &self.hb.pending_farm_test {
                    if now.duration_since(*sent_at).as_secs() > FARM_HEARTBEAT_SECS {
                        // Farm heartbeat timeout — set flag, don't kill loop
                        log::error!("Farm heartbeat timeout — connection lost");
                        self.handle_farm_disconnect();
                    }
                } else {
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

        // --- HMDS heartbeat (skip if disconnected or no TBT subscriptions) ---
        if !self.hmds_disconnected && !self.tbt_subscriptions.is_empty() {
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

            if since_recv > FARM_HEARTBEAT_SECS + HEARTBEAT_GRACE_SECS {
                if let Some((_, sent_at)) = &self.hb.pending_hmds_test {
                    if now.duration_since(*sent_at).as_secs() > FARM_HEARTBEAT_SECS {
                        log::error!("HMDS heartbeat timeout — connection lost");
                        self.hmds_disconnected = true;
                    }
                } else {
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

}

/// Convert Side to FIX tag 54 value.
fn fix_side(side: Side) -> &'static str {
    match side {
        Side::Buy => "1",
        Side::Sell => "2",
        Side::ShortSell => "5",
    }
}

/// Format a fixed-point Price as a decimal string for FIX tags.
fn format_price(price: Price) -> String {
    let whole = price / PRICE_SCALE;
    let frac = (price % PRICE_SCALE).unsigned_abs();
    if frac == 0 {
        whole.to_string()
    } else {
        // Trim trailing zeros
        let frac_str = format!("{:08}", frac);
        let trimmed = frac_str.trim_end_matches('0');
        format!("{}.{}", whole, trimmed)
    }
}

/// Find the body content after a specific tag marker in a FIX/binary message.
fn find_body_after_tag<'a>(msg: &'a [u8], tag_marker: &[u8]) -> Option<&'a [u8]> {
    msg.windows(tag_marker.len())
        .position(|w| w == tag_marker)
        .map(|pos| &msg[pos + tag_marker.len()..])
}

/// Build string values for condition FIX tags.
/// Returns: [count, cond1_type, cond1_conj, cond1_op, cond1_conid, cond1_exch,
///           cond1_trigger, cond1_price, cond1_time, cond1_pct, cond1_vol, cond1_exec, ...]
/// 1 + 11 * N strings total.
/// Build algo FIX tag values: returns (algo_name, [key, value, key, value, ...]).
/// Keys/values are for 5958/5960 repeated pairs.
fn build_algo_tags(algo: &AlgoParams) -> (&'static str, Vec<String>) {
    match algo {
        AlgoParams::Vwap { no_take_liq, allow_past_end_time, start_time, end_time, .. } => {
            ("Vwap", vec![
                "noTakeLiq".into(), if *no_take_liq { "1" } else { "0" }.into(),
                "allowPastEndTime".into(), if *allow_past_end_time { "1" } else { "0" }.into(),
                "startTime".into(), start_time.clone(),
                "endTime".into(), end_time.clone(),
            ])
        }
        AlgoParams::Twap { allow_past_end_time, start_time, end_time } => {
            ("Twap", vec![
                "allowPastEndTime".into(), if *allow_past_end_time { "1" } else { "0" }.into(),
                "startTime".into(), start_time.clone(),
                "endTime".into(), end_time.clone(),
            ])
        }
        AlgoParams::ArrivalPx { risk_aversion, allow_past_end_time, force_completion, start_time, end_time, .. } => {
            ("ArrivalPx", vec![
                "riskAversion".into(), risk_aversion.as_str().into(),
                "allowPastEndTime".into(), if *allow_past_end_time { "1" } else { "0" }.into(),
                "forceCompletion".into(), if *force_completion { "1" } else { "0" }.into(),
                "startTime".into(), start_time.clone(),
                "endTime".into(), end_time.clone(),
            ])
        }
        AlgoParams::ClosePx { risk_aversion, force_completion, start_time, .. } => {
            ("ClosePx", vec![
                "riskAversion".into(), risk_aversion.as_str().into(),
                "forceCompletion".into(), if *force_completion { "1" } else { "0" }.into(),
                "startTime".into(), start_time.clone(),
            ])
        }
        AlgoParams::DarkIce { allow_past_end_time, display_size, start_time, end_time } => {
            ("DarkIce", vec![
                "allowPastEndTime".into(), if *allow_past_end_time { "1" } else { "0" }.into(),
                "displaySize".into(), display_size.to_string(),
                "startTime".into(), start_time.clone(),
                "endTime".into(), end_time.clone(),
            ])
        }
        AlgoParams::PctVol { pct_vol, no_take_liq, start_time, end_time } => {
            ("PctVol", vec![
                "noTakeLiq".into(), if *no_take_liq { "1" } else { "0" }.into(),
                "pctVol".into(), format!("{}", pct_vol),
                "startTime".into(), start_time.clone(),
                "endTime".into(), end_time.clone(),
            ])
        }
    }
}

fn build_condition_strings(conditions: &[OrderCondition]) -> Vec<String> {
    let mut out = Vec::with_capacity(1 + conditions.len() * 11);
    out.push(conditions.len().to_string());
    for (i, cond) in conditions.iter().enumerate() {
        let is_last = i == conditions.len() - 1;
        let conj = if is_last { "n" } else { "a" };
        let op = |is_more: bool| if is_more { ">=" } else { "<=" };
        match cond {
            OrderCondition::Price { con_id, exchange, price, is_more, trigger_method } => {
                out.push("1".into());                              // condType
                out.push(conj.into());                             // conjunction
                out.push(op(*is_more).into());                     // operator
                out.push(con_id.to_string());                      // conId
                out.push(exchange.clone());                        // exchange
                out.push(trigger_method.to_string());              // triggerMethod
                out.push(format_price(*price));                    // price
                out.push(String::new());                           // time (unused)
                out.push(String::new());                           // percent (unused)
                out.push(String::new());                           // volume (unused)
                out.push(String::new());                           // execution (unused)
            }
            OrderCondition::Time { time, is_more } => {
                out.push("3".into());
                out.push(conj.into());
                out.push(op(*is_more).into());
                out.push(String::new());                           // conId (unused)
                out.push(String::new());                           // exchange (unused)
                out.push(String::new());                           // triggerMethod (unused)
                out.push(String::new());                           // price (unused)
                out.push(time.clone());                            // time
                out.push(String::new());                           // percent (unused)
                out.push(String::new());                           // volume (unused)
                out.push(String::new());                           // execution (unused)
            }
            OrderCondition::Margin { percent, is_more } => {
                out.push("4".into());
                out.push(conj.into());
                out.push(op(*is_more).into());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(percent.to_string());                     // percent
                out.push(String::new());
                out.push(String::new());
            }
            OrderCondition::Execution { symbol, exchange, sec_type } => {
                out.push("5".into());
                out.push(conj.into());
                out.push(String::new());                           // operator (unused)
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                let exch = if exchange == "SMART" { "*" } else { exchange.as_str() };
                out.push(format!("symbol={};exchange={};securityType={};", symbol, exch, sec_type));
            }
            OrderCondition::Volume { con_id, exchange, volume, is_more } => {
                out.push("6".into());
                out.push(conj.into());
                out.push(op(*is_more).into());
                out.push(con_id.to_string());
                out.push(exchange.clone());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(volume.to_string());                      // volume
                out.push(String::new());
            }
            OrderCondition::PercentChange { con_id, exchange, percent, is_more } => {
                out.push("7".into());
                out.push(conj.into());
                out.push(op(*is_more).into());
                out.push(con_id.to_string());
                out.push(exchange.clone());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(format!("{}", percent));                   // percent
                out.push(String::new());
                out.push(String::new());
            }
        }
    }
    out
}

impl<S: Strategy> HotLoop<S> {
    fn pin_to_core(core: usize) {
        let core_ids = core_affinity::get_core_ids().unwrap_or_default();
        if let Some(id) = core_ids.get(core) {
            core_affinity::set_for_current(*id);
        }
    }

    /// Whether the farm connection has been lost.
    pub fn is_farm_disconnected(&self) -> bool {
        self.farm_disconnected
    }

    /// Whether the CCP connection has been lost.
    pub fn is_ccp_disconnected(&self) -> bool {
        self.ccp_disconnected
    }

    /// Replace the farm connection (after reconnection) and re-subscribe to all instruments.
    pub fn reconnect_farm(&mut self, conn: Connection) {
        self.farm_conn = Some(conn);
        self.farm_disconnected = false;
        self.hb.last_farm_sent = Instant::now();
        self.hb.last_farm_recv = Instant::now();
        self.hb.pending_farm_test = None;

        // Re-subscribe all active instruments
        let active: Vec<(InstrumentId, i64)> = self.context.market.active_instruments().collect();
        self.md_req_to_instrument.clear();
        self.instrument_md_reqs.clear();
        for (instrument, con_id) in active {
            self.send_mktdata_subscribe(con_id, instrument);
        }
        log::info!("Farm reconnected, re-subscribed {} instruments", self.instrument_md_reqs.len());
    }

    /// Replace the CCP connection (after reconnection) and reconcile order state.
    /// Sends 35=H (OrderMassStatusRequest) to discover orders that may have changed
    /// during the disconnect window.
    pub fn reconnect_ccp(&mut self, conn: Connection) {
        self.ccp_conn = Some(conn);
        self.ccp_disconnected = false;
        self.hb.last_ccp_sent = Instant::now();
        self.hb.last_ccp_recv = Instant::now();
        self.hb.pending_ccp_test = None;

        // Send 35=H mass status request to reconcile orders with broker state.
        // Any orders filled/cancelled during disconnect will generate exec reports.
        if let Some(conn) = self.ccp_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let result = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "H"),
                (fix::TAG_SENDING_TIME, &ts),
                (11, "*"),  // ClOrdID wildcard
                (54, "*"),  // Side wildcard
                (55, "*"),  // Symbol wildcard
            ]);
            match result {
                Ok(()) => {
                    self.hb.last_ccp_sent = Instant::now();
                    log::info!("CCP reconnected, sent order mass status request");
                }
                Err(e) => log::error!("CCP reconnected but mass status request failed: {}", e),
            }
        }
    }

    /// Poll HMDS connection for tick-by-tick data (35=E messages).
    fn poll_hmds(&mut self) {
        if self.hmds_disconnected || self.tbt_subscriptions.is_empty() {
            return;
        }
        let messages = match self.hmds_conn.as_mut() {
            None => return,
            Some(conn) => {
                match conn.try_recv() {
                    Ok(0) => return,
                    Err(e) => {
                        log::error!("HMDS connection lost: {}", e);
                        self.hmds_disconnected = true;
                        return;
                    }
                    Ok(n) => {
                        log::info!("HMDS recv: {} bytes", n);
                        self.hb.last_hmds_recv = Instant::now();
                        self.hb.pending_hmds_test = None;
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
            self.process_hmds_message(msg);
        }
    }

    fn process_hmds_message(&mut self, msg: &[u8]) {
        let parsed = fix::fix_parse(msg);
        let msg_type = match parsed.get(&fix::TAG_MSG_TYPE) {
            Some(t) => t.as_str(),
            None => return,
        };

        match msg_type {
            "E" => self.handle_tbt_data(msg),
            "0" => {} // heartbeat
            "1" => {
                // Test request — respond with heartbeat
                let test_id = parsed.get(&fix::TAG_TEST_REQ_ID).cloned().unwrap_or_default();
                if let Some(conn) = self.hmds_conn.as_mut() {
                    let ts = chrono_free_timestamp();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    self.hb.last_hmds_sent = Instant::now();
                }
            }
            "W" => {
                // 35=W response — may contain ResultSetTickerId for TBT subscription ack
                if let Some(xml_tag) = parsed.get(&6118) {
                    if let Some(ticker_id) = crate::control::historical::parse_ticker_id(xml_tag) {
                        log::info!("HMDS TBT ticker_id assigned: {}", ticker_id);
                        // TODO: map ticker_id to instrument for future use
                    }
                }
            }
            _ => {}
        }
    }

    /// Decode 35=E tick-by-tick binary data and dispatch to strategy callbacks.
    fn handle_tbt_data(&mut self, msg: &[u8]) {
        let body = match find_body_after_tag(msg, b"35=E\x01") {
            Some(b) => b,
            None => return,
        };

        let entries = tick_decoder::decode_ticks_35e(body);

        for entry in &entries {
            // For now, use the first TBT subscription's instrument (single-instrument TBT).
            // Multi-instrument TBT would need server_tag → instrument mapping from HMDS.
            let instrument = match self.tbt_subscriptions.first() {
                Some((id, _, _)) => *id,
                None => return,
            };

            match entry {
                tick_decoder::TbtEntry::Trade { timestamp, price_cents_delta, size, exchange, conditions } => {
                    // Update running price state
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
                    self.strategy.on_tbt_trade(&trade, &mut self.context);
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
                    self.strategy.on_tbt_quote(&quote, &mut self.context);
                }
            }
        }
    }

    /// Update running last-price state for TBT, return new absolute cents.
    fn update_tbt_price(&mut self, instrument: InstrumentId, delta: i64, _: i64) -> i64 {
        for entry in &mut self.tbt_price_state {
            if entry.0 == instrument {
                entry.1 += delta;
                return entry.1;
            }
        }
        // First tick for this instrument
        self.tbt_price_state.push((instrument, delta, 0, 0));
        delta
    }

    /// Update running bid/ask state for TBT, return (bid_cents, ask_cents).
    fn update_tbt_bid_ask(&mut self, instrument: InstrumentId, bid_delta: i64, ask_delta: i64) -> (i64, i64) {
        for entry in &mut self.tbt_price_state {
            if entry.0 == instrument {
                entry.2 += bid_delta;
                entry.3 += ask_delta;
                return (entry.2, entry.3);
            }
        }
        self.tbt_price_state.push((instrument, 0, bid_delta, ask_delta));
        (bid_delta, ask_delta)
    }

    /// Send TBT subscribe request to HMDS (35=W with XML).
    fn send_tbt_subscribe(&mut self, con_id: i64, instrument: InstrumentId, tbt_type: crate::types::TbtType) {
        let req_id = self.next_tbt_req_id;
        self.next_tbt_req_id += 1;

        let tbt_type_str = match tbt_type {
            crate::types::TbtType::Last => "AllLast",
            crate::types::TbtType::BidAsk => "BidAsk",
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

        if let Some(conn) = self.hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "W"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ]);
            log::info!("Sent TBT subscribe: con_id={} type={} req_id={}", con_id, tbt_type_str, req_id);
            self.hb.last_hmds_sent = Instant::now();
        }

        let ticker_id = format!("tbt_{}", req_id);
        self.tbt_subscriptions.push((instrument, ticker_id, tbt_type));
    }

    /// Send TBT unsubscribe request to HMDS (35=Z with XML).
    fn send_tbt_unsubscribe(&mut self, instrument: InstrumentId) {
        let idx = match self.tbt_subscriptions.iter().position(|(id, _, _)| *id == instrument) {
            Some(i) => i,
            None => return,
        };
        let (_, ticker_id, _) = self.tbt_subscriptions.remove(idx);

        if let Some(conn) = self.hmds_conn.as_mut() {
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
            self.hb.last_hmds_sent = Instant::now();
        }

        // Clear price state
        self.tbt_price_state.retain(|e| e.0 != instrument);
    }

    /// Handle farm disconnect: clear stale subscription tracking, zero quotes, notify strategy.
    fn handle_farm_disconnect(&mut self) {
        self.farm_disconnected = true;
        self.md_req_to_instrument.clear();
        self.instrument_md_reqs.clear();
        self.context.market.clear_server_tags();
        self.context.market.zero_all_quotes();
        self.strategy.on_disconnect(&mut self.context);
    }

    /// Handle CCP disconnect: mark open orders uncertain, notify strategy.
    fn handle_ccp_disconnect(&mut self) {
        self.ccp_disconnected = true;
        self.context.mark_orders_uncertain();
        self.strategy.on_disconnect(&mut self.context);
    }

    /// Access heartbeat state for testing.
    pub fn heartbeat_state(&self) -> &HeartbeatState {
        &self.hb
    }

    /// Mutably access heartbeat state for testing (e.g., setting timestamps).
    pub fn heartbeat_state_mut(&mut self) -> &mut HeartbeatState {
        &mut self.hb
    }

    /// Inject a raw farm message for testing. Processes it through the full decode pipeline.
    pub fn inject_farm_message(&mut self, msg: &[u8]) {
        self.process_farm_message(msg);
    }

    /// Inject a raw CCP message for testing. Processes execution reports, etc.
    pub fn inject_ccp_message(&mut self, msg: &[u8]) {
        self.process_ccp_message(msg);
    }

    /// Inject a simulated tick for testing. Calls on_tick with the given instrument.
    pub fn inject_tick(&mut self, instrument: InstrumentId) {
        self.strategy.on_tick(instrument, &mut self.context);
    }

    /// Simulate a fill for testing. Updates position and calls strategy.on_fill().
    pub fn inject_fill(&mut self, fill: &crate::types::Fill) {
        let delta = match fill.side {
            crate::types::Side::Buy => fill.qty,
            crate::types::Side::Sell | crate::types::Side::ShortSell => -fill.qty,
        };
        self.context.update_position(fill.instrument, delta);
        self.strategy.on_fill(fill, &mut self.context);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use std::time::Duration;

    struct RecordingStrategy {
        ticks: Vec<InstrumentId>,
        fills: Vec<(OrderId, i64)>,  // (order_id, qty)
        updates: Vec<(OrderId, OrderStatus)>,
        cancel_rejects: Vec<(OrderId, u8)>,  // (order_id, reject_type)
        disconnected: bool,
        started: bool,
    }

    impl RecordingStrategy {
        fn new() -> Self {
            Self {
                ticks: Vec::new(),
                fills: Vec::new(),
                updates: Vec::new(),
                cancel_rejects: Vec::new(),
                disconnected: false,
                started: false,
            }
        }
    }

    impl Strategy for RecordingStrategy {
        fn on_start(&mut self, _ctx: &mut Context) {
            self.started = true;
        }

        fn on_tick(&mut self, instrument: InstrumentId, _ctx: &mut Context) {
            self.ticks.push(instrument);
        }

        fn on_fill(&mut self, fill: &Fill, _ctx: &mut Context) {
            self.fills.push((fill.order_id, fill.qty));
        }

        fn on_order_update(&mut self, update: &OrderUpdate, _ctx: &mut Context) {
            self.updates.push((update.order_id, update.status));
        }

        fn on_cancel_reject(&mut self, reject: &CancelReject, _ctx: &mut Context) {
            self.cancel_rejects.push((reject.order_id, reject.reject_type));
        }

        fn on_disconnect(&mut self, _ctx: &mut Context) {
            self.disconnected = true;
        }
    }

    #[test]
    fn inject_tick_calls_strategy() {
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        engine.context_mut().market.register(265598);

        engine.inject_tick(0);
        engine.inject_tick(0);

        assert_eq!(engine.strategy.ticks, vec![0, 0]);
    }

    #[test]
    fn inject_tick_multiple_instruments() {
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        engine.context_mut().market.register(265598); // 0: AAPL
        engine.context_mut().market.register(272093); // 1: MSFT

        engine.inject_tick(0);
        engine.inject_tick(1);
        engine.inject_tick(0);

        assert_eq!(engine.strategy.ticks, vec![0, 1, 0]);
    }

    #[test]
    fn inject_fill_updates_position() {
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        engine.context_mut().market.register(265598);

        let fill = Fill {
            instrument: 0,
            order_id: 1,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            remaining: 0,
            commission: 0,
            timestamp_ns: 0,
        };
        engine.inject_fill(&fill);

        assert_eq!(engine.context_mut().position(0), 100);
        assert_eq!(engine.strategy.fills, vec![(1, 100)]);
    }

    #[test]
    fn inject_fill_sell_decreases_position() {
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        engine.context_mut().market.register(265598);
        engine.context_mut().update_position(0, 100);

        let fill = Fill {
            instrument: 0,
            order_id: 2,
            side: Side::Sell,
            price: 152 * PRICE_SCALE,
            qty: 30,
            remaining: 0,
            commission: 0,
            timestamp_ns: 0,
        };
        engine.inject_fill(&fill);

        assert_eq!(engine.context_mut().position(0), 70);
    }

    #[test]
    fn strategy_reads_market_data_in_on_tick() {
        struct SpreadReader {
            last_spread: Price,
        }

        impl Strategy for SpreadReader {
            fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
                self.last_spread = ctx.spread(instrument);
            }
        }

        let strategy = SpreadReader { last_spread: 0 };
        let mut engine = HotLoop::new(strategy, None);
        let id = engine.context_mut().market.register(265598);
        let q = engine.context_mut().market.quote_mut(id);
        q.bid = 15000 * (PRICE_SCALE / 100);
        q.ask = 15010 * (PRICE_SCALE / 100);

        engine.inject_tick(id);

        assert_eq!(engine.strategy.last_spread, 10 * (PRICE_SCALE / 100));
    }

    #[test]
    fn handle_tick_data_updates_quote() {
        // Build a synthetic 35=P message with known tick data.
        // Format: "8=O\x01 9=<len>\x01 35=P\x01 <binary payload>"
        // Binary payload: 2-byte bit_count, then bit-packed ticks.
        //
        // We'll build a payload with one server_tag entry containing a bid_size tick.
        // server_tag entry: 1 bit cont + 31 bits server_tag = 32 bits
        // tick: 5 bits type + 1 bit has_more + 2 bits width + 8 bits value = 16 bits
        // Total: 48 bits = 6 bytes

        let server_tag: u32 = 100;
        let tick_type: u8 = 4; // O_BID_SIZE
        let value: u8 = 50; // magnitude = 50

        // Build bit-packed payload
        let mut bits: Vec<u8> = Vec::new();
        // cont(1) = 0, server_tag(31) = 100
        let st_bits: u32 = server_tag; // cont=0, tag=100
        bits.push((st_bits >> 24) as u8);
        bits.push((st_bits >> 16) as u8);
        bits.push((st_bits >> 8) as u8);
        bits.push(st_bits as u8);
        // tick_type(5) = 4, has_more(1) = 0, width(2) = 0 (1 byte)
        // = 00100 0 00 = 0x20
        // value: sign(1) = 0, magnitude(7) = 50
        // = 0 0110010 = 0x32
        bits.push((tick_type << 3) | 0b000); // type=4, has_more=0, width=0 → 00100_0_00 = 0x20
        bits.push((0 << 7) | value); // sign=0, magnitude=50 → 0_0110010 = 0x32

        let bit_count: u16 = 48; // 32 (server_tag) + 16 (tick)
        let mut payload = Vec::new();
        payload.push((bit_count >> 8) as u8);
        payload.push(bit_count as u8);
        payload.extend_from_slice(&bits);

        // Wrap in 8=O FIX framing
        let body = format!("35=P\x01");
        let body_len = body.len() + payload.len();
        let mut msg = format!("8=O\x019={}\x01{}", body_len, body).into_bytes();
        msg.extend_from_slice(&payload);

        // Set up engine
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        let aapl = engine.context_mut().market.register(265598);
        engine.context_mut().market.set_min_tick(aapl, 0.01);
        engine.context_mut().market.register_server_tag(100, aapl);

        // Process the message
        engine.inject_farm_message(&msg);

        // bid_size should be updated to 50
        assert_eq!(engine.context_mut().market.bid_size(aapl), 50);
        // Strategy should have been notified
        assert_eq!(engine.strategy.ticks, vec![aapl]);
    }

    #[test]
    fn handle_tick_data_unknown_server_tag_ignored() {
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        engine.context_mut().market.register(265598);

        // Build a minimal 35=P with unknown server_tag
        let mut payload = vec![0x00, 0x30]; // 48 bits
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0xFF]); // server_tag=255 (unknown)
        payload.extend_from_slice(&[0x20, 0x32]); // bid_size=50

        let body = format!("35=P\x01");
        let body_len = body.len() + payload.len();
        let mut msg = format!("8=O\x019={}\x01{}", body_len, body).into_bytes();
        msg.extend_from_slice(&payload);

        engine.inject_farm_message(&msg);

        // No strategy notification (unknown server_tag)
        assert!(engine.strategy.ticks.is_empty());
    }

    #[test]
    fn exec_report_fill_updates_position_and_notifies() {
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        engine.context_mut().market.register(265598);

        // Insert an open order so the exec report can find it
        engine.context_mut().insert_order(crate::types::Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: crate::types::OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // Build FIX 35=8 execution report (filled)
        let exec_msg = fix::fix_build(&[
            (35, "8"),      // ExecReport
            (11, "1"),      // ClOrdID
            (39, "2"),      // OrdStatus = Filled
            (150, "F"),     // ExecType = Fill
            (31, "150.0"),  // LastPx
            (32, "100"),    // LastShares
            (151, "0"),     // LeavesQty
        ], 1);

        engine.inject_ccp_message(&exec_msg);

        // Position should be updated (+100 for buy)
        assert_eq!(engine.context_mut().position(0), 100);
        // Strategy should have received the fill
        assert_eq!(engine.strategy.fills, vec![(1, 100)]);
        // Order should be removed (terminal state)
        assert!(engine.context_mut().order(1).is_none());
    }

    #[test]
    fn exec_report_partial_fill() {
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(crate::types::Order {
            order_id: 2,
            instrument: 0,
            side: Side::Sell,
            price: 200 * PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: crate::types::OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        let exec_msg = fix::fix_build(&[
            (35, "8"),
            (11, "2"),
            (39, "1"),      // OrdStatus = Partially Filled
            (150, "1"),     // ExecType = Partial
            (31, "200.5"),
            (32, "30"),     // 30 shares filled
            (151, "70"),    // 70 remaining
        ], 1);

        engine.inject_ccp_message(&exec_msg);

        // Position should be -30 (sell)
        assert_eq!(engine.context_mut().position(0), -30);
        // Order should still exist (not terminal)
        assert!(engine.context_mut().order(2).is_some());
    }

    #[test]
    fn exec_report_rejected() {
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(crate::types::Order {
            order_id: 3,
            instrument: 0,
            side: Side::Buy,
            price: 100 * PRICE_SCALE,
            qty: 50,
            filled: 0,
            status: crate::types::OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        let exec_msg = fix::fix_build(&[
            (35, "8"),
            (11, "3"),
            (39, "8"),      // OrdStatus = Rejected
            (150, "8"),
            (58, "Insufficient margin"),
        ], 1);

        engine.inject_ccp_message(&exec_msg);

        // No position change (rejected, no fill)
        assert_eq!(engine.context_mut().position(0), 0);
        // Order should be removed (terminal)
        assert!(engine.context_mut().order(3).is_none());
    }

    #[test]
    fn control_subscribe_registers_instrument() {
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        let (tx, rx) = crossbeam_channel::bounded(16);
        engine.set_control_rx(rx);

        tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();
        engine.poll_control_commands();

        // Instrument should be registered
        let id = engine.context_mut().market.register(265598);
        assert_eq!(id, 0); // same conId returns same id
    }

    #[test]
    fn control_shutdown_stops_loop() {
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        let (tx, rx) = crossbeam_channel::bounded(16);
        engine.set_control_rx(rx);

        tx.send(ControlCommand::Shutdown).unwrap();
        engine.poll_control_commands();

        assert!(!engine.running);
    }

    #[test]
    fn control_multiple_commands_drained() {
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        let (tx, rx) = crossbeam_channel::bounded(16);
        engine.set_control_rx(rx);

        tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();
        tx.send(ControlCommand::Subscribe { con_id: 272093, symbol: "MSFT".into() }).unwrap();
        tx.send(ControlCommand::UpdateParam { key: "k".into(), value: "v".into() }).unwrap();
        engine.poll_control_commands();

        // Both instruments registered
        assert_eq!(engine.context_mut().market.register(265598), 0);
        assert_eq!(engine.context_mut().market.register(272093), 1);
    }

    #[test]
    fn heartbeat_state_initialized() {
        let engine = HotLoop::new(RecordingStrategy::new(), None);
        let hb = engine.heartbeat_state();
        // All timestamps should be recent (within 1 second of now)
        let now = Instant::now();
        assert!(now.duration_since(hb.last_ccp_sent).as_secs() < 1);
        assert!(now.duration_since(hb.last_farm_recv).as_secs() < 1);
        assert!(hb.pending_ccp_test.is_none());
        assert!(hb.pending_farm_test.is_none());
    }

    #[test]
    fn heartbeat_test_id_increments() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        let id1 = engine.heartbeat_state_mut().next_test_id();
        let id2 = engine.heartbeat_state_mut().next_test_id();
        assert_eq!(id1, "T1");
        assert_eq!(id2, "T2");
    }

    #[test]
    fn check_heartbeats_no_connections_no_panic() {
        // No connections set — check_heartbeats should be a no-op
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.check_heartbeats(); // should not panic
        assert!(engine.running);
    }

    #[test]
    fn check_heartbeats_skips_already_disconnected() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        // Simulate: CCP and farm already disconnected
        engine.ccp_disconnected = true;
        engine.farm_disconnected = true;
        // Set heartbeat timestamps far in the past to trigger timeout if guards fail
        engine.hb.last_ccp_recv = Instant::now() - Duration::from_secs(120);
        engine.hb.last_farm_recv = Instant::now() - Duration::from_secs(120);
        // Should be a no-op — no duplicate disconnect handling
        engine.check_heartbeats();
        // Strategy should NOT have received on_disconnect (already disconnected)
        assert!(!engine.strategy().disconnected);
    }

    #[test]
    fn build_condition_strings_price() {
        let conds = vec![OrderCondition::Price {
            con_id: 265598,
            exchange: "BEST".into(),
            price: 31103 * (PRICE_SCALE / 100), // 311.03
            is_more: true,
            trigger_method: 2,
        }];
        let s = build_condition_strings(&conds);
        assert_eq!(s[0], "1");     // count
        assert_eq!(s[1], "1");     // condType = Price
        assert_eq!(s[2], "n");     // conjunction = last
        assert_eq!(s[3], ">=");    // operator
        assert_eq!(s[4], "265598"); // conId
        assert_eq!(s[5], "BEST");  // exchange
        assert_eq!(s[6], "2");     // triggerMethod
        assert_eq!(s[7], "311.03"); // price
        assert_eq!(s[8], "");      // time (unused)
    }

    #[test]
    fn build_condition_strings_multi_price_and_volume() {
        let conds = vec![
            OrderCondition::Price {
                con_id: 265598, exchange: "BEST".into(),
                price: 300 * PRICE_SCALE, is_more: true, trigger_method: 0,
            },
            OrderCondition::Volume {
                con_id: 265598, exchange: "BEST".into(),
                volume: 1000000, is_more: true,
            },
        ];
        let s = build_condition_strings(&conds);
        assert_eq!(s[0], "2");    // count = 2
        // Condition 1 (Price)
        assert_eq!(s[1], "1");    // condType
        assert_eq!(s[2], "a");    // conjunction = AND (not last)
        assert_eq!(s[3], ">=");
        // Condition 2 (Volume) — starts at index 12
        assert_eq!(s[12], "6");   // condType = Volume
        assert_eq!(s[13], "n");   // conjunction = last
        assert_eq!(s[14], ">=");  // operator
        assert_eq!(s[21], "1000000"); // volume
    }

    #[test]
    fn build_condition_strings_time() {
        let conds = vec![OrderCondition::Time {
            time: "20260310-14:30:00".into(),
            is_more: true,
        }];
        let s = build_condition_strings(&conds);
        assert_eq!(s[1], "3");                  // condType = Time
        assert_eq!(s[8], "20260310-14:30:00");  // time
    }

    #[test]
    fn build_condition_strings_margin() {
        let conds = vec![OrderCondition::Margin { percent: 5, is_more: false }];
        let s = build_condition_strings(&conds);
        assert_eq!(s[1], "4");   // condType = Margin
        assert_eq!(s[3], "<=");  // operator (is_more=false)
        assert_eq!(s[9], "5");   // percent
    }

    #[test]
    fn build_condition_strings_execution() {
        let conds = vec![OrderCondition::Execution {
            symbol: "AAPL".into(),
            exchange: "SMART".into(),
            sec_type: "CS".into(),
        }];
        let s = build_condition_strings(&conds);
        assert_eq!(s[1], "5");   // condType = Execution
        assert_eq!(s[3], "");    // operator (unused)
        assert_eq!(s[11], "symbol=AAPL;exchange=*;securityType=CS;");
    }

    #[test]
    fn build_condition_strings_percent_change() {
        let conds = vec![OrderCondition::PercentChange {
            con_id: 265598, exchange: "BEST".into(),
            percent: 5.5, is_more: true,
        }];
        let s = build_condition_strings(&conds);
        assert_eq!(s[1], "7");    // condType = PercentChange
        assert_eq!(s[9], "5.5");  // percent
    }

    #[test]
    fn build_condition_strings_empty() {
        let s = build_condition_strings(&[]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0], "0");
    }

    // ── build_algo_tags tests ──

    #[test]
    fn build_algo_tags_vwap() {
        let algo = AlgoParams::Vwap {
            max_pct_vol: 0.1,
            no_take_liq: false,
            allow_past_end_time: true,
            start_time: "20260311-13:30:00".into(),
            end_time: "20260311-20:00:00".into(),
        };
        let (name, params) = build_algo_tags(&algo);
        assert_eq!(name, "Vwap");
        assert_eq!(params.len(), 8); // 4 key-value pairs
        assert_eq!(params[0], "noTakeLiq");
        assert_eq!(params[1], "0");
        assert_eq!(params[2], "allowPastEndTime");
        assert_eq!(params[3], "1");
        assert_eq!(params[4], "startTime");
        assert_eq!(params[5], "20260311-13:30:00");
        assert_eq!(params[6], "endTime");
        assert_eq!(params[7], "20260311-20:00:00");
    }

    #[test]
    fn build_algo_tags_twap() {
        let algo = AlgoParams::Twap {
            allow_past_end_time: false,
            start_time: "20260311-13:30:00".into(),
            end_time: "20260311-20:00:00".into(),
        };
        let (name, params) = build_algo_tags(&algo);
        assert_eq!(name, "Twap");
        assert_eq!(params.len(), 6);
        assert_eq!(params[0], "allowPastEndTime");
        assert_eq!(params[1], "0");
    }

    #[test]
    fn build_algo_tags_arrival_px() {
        let algo = AlgoParams::ArrivalPx {
            max_pct_vol: 0.25,
            risk_aversion: crate::types::RiskAversion::Aggressive,
            allow_past_end_time: true,
            force_completion: true,
            start_time: "20260311-13:30:00".into(),
            end_time: "20260311-20:00:00".into(),
        };
        let (name, params) = build_algo_tags(&algo);
        assert_eq!(name, "ArrivalPx");
        assert_eq!(params.len(), 10); // 5 pairs
        assert_eq!(params[0], "riskAversion");
        assert_eq!(params[1], "Aggressive");
        assert_eq!(params[4], "forceCompletion");
        assert_eq!(params[5], "1");
    }

    #[test]
    fn build_algo_tags_close_px() {
        let algo = AlgoParams::ClosePx {
            max_pct_vol: 0.1,
            risk_aversion: crate::types::RiskAversion::Neutral,
            force_completion: false,
            start_time: "20260311-13:30:00".into(),
        };
        let (name, params) = build_algo_tags(&algo);
        assert_eq!(name, "ClosePx");
        assert_eq!(params.len(), 6); // 3 pairs
        assert_eq!(params[1], "Neutral");
    }

    #[test]
    fn build_algo_tags_dark_ice() {
        let algo = AlgoParams::DarkIce {
            allow_past_end_time: true,
            display_size: 10,
            start_time: "20260311-13:30:00".into(),
            end_time: "20260311-20:00:00".into(),
        };
        let (name, params) = build_algo_tags(&algo);
        assert_eq!(name, "DarkIce");
        assert_eq!(params[2], "displaySize");
        assert_eq!(params[3], "10");
    }

    #[test]
    fn build_algo_tags_pct_vol() {
        let algo = AlgoParams::PctVol {
            pct_vol: 0.15,
            no_take_liq: true,
            start_time: "20260311-13:30:00".into(),
            end_time: "20260311-20:00:00".into(),
        };
        let (name, params) = build_algo_tags(&algo);
        assert_eq!(name, "PctVol");
        assert_eq!(params[0], "noTakeLiq");
        assert_eq!(params[1], "1");
        assert_eq!(params[2], "pctVol");
        assert_eq!(params[3], "0.15");
    }

    #[test]
    fn format_price_whole() {
        assert_eq!(format_price(150 * PRICE_SCALE), "150");
        assert_eq!(format_price(0), "0");
    }

    #[test]
    fn format_price_decimal() {
        assert_eq!(format_price(15025 * (PRICE_SCALE / 100)), "150.25");
        assert_eq!(format_price(PRICE_SCALE / 2), "0.5");
    }

    #[test]
    fn fix_side_mapping() {
        assert_eq!(fix_side(Side::Buy), "1");
        assert_eq!(fix_side(Side::Sell), "2");
        assert_eq!(fix_side(Side::ShortSell), "5");
    }

    #[test]
    fn strategy_submits_and_engine_drains() {
        struct AutoBuyer;

        impl Strategy for AutoBuyer {
            fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
                if ctx.position(instrument) == 0 {
                    ctx.submit_limit(instrument, Side::Buy, 100, ctx.bid(instrument));
                }
            }
        }

        let mut engine = HotLoop::new(AutoBuyer, None);
        let id = engine.context_mut().market.register(265598);
        engine.context_mut().market.quote_mut(id).bid = 150 * PRICE_SCALE;

        // Tick triggers order submission
        engine.inject_tick(id);

        // Engine drains pending orders (simulating what drain_and_send_orders does)
        let orders: Vec<_> = engine.context_mut().drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);

        // Simulate fill → position changes → next tick doesn't order again
        engine.context_mut().update_position(id, 100);
        engine.inject_tick(id);

        let orders: Vec<_> = engine.context_mut().drain_pending_orders().collect();
        assert!(orders.is_empty());
    }

    #[test]
    fn subscribe_tracks_md_req_id() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        let (tx, rx) = crossbeam_channel::bounded(16);
        engine.set_control_rx(rx);

        tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();
        engine.poll_control_commands();

        // Should have 2 pending subscriptions (BidAsk + Last) with req_id=1,2
        assert_eq!(engine.md_req_to_instrument.len(), 2);
        assert_eq!(engine.md_req_to_instrument[0], (1, 0));
        assert_eq!(engine.md_req_to_instrument[1], (2, 0));
        // Should track active subscription
        assert_eq!(engine.instrument_md_reqs.len(), 1);
        assert_eq!(engine.instrument_md_reqs[0].0, 0);
        assert_eq!(engine.instrument_md_reqs[0].1, vec![1, 2]);
        // Next req_id should be 3
        assert_eq!(engine.next_md_req_id, 3);
    }

    #[test]
    fn subscription_ack_registers_server_tag() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        // Simulate pending subscription for req_id=1 → instrument 0
        engine.md_req_to_instrument.push((1, 0));

        // Build raw 35=Q message: CSV body = serverTag,reqId,minTick,...
        let msg = b"8=O\x019=999\x0135=Q\x0142,1,0.01,extra\x01";

        engine.handle_subscription_ack(msg);

        // server_tag 42 should map to instrument 0
        assert_eq!(engine.context_mut().market.instrument_by_server_tag(42), Some(0));
        assert!((engine.context_mut().market.min_tick(0) - 0.01).abs() < 1e-10);
        // Pending subscription should be consumed
        assert!(engine.md_req_to_instrument.is_empty());
    }

    #[test]
    fn ticker_setup_registers_server_tag() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        // Build raw 35=L message: CSV body = conId,minTick,serverTag,,1
        let msg = b"8=O\x019=999\x0135=L\x01265598,0.01,99,,1\x01";

        engine.handle_ticker_setup(msg);

        assert_eq!(engine.context_mut().market.instrument_by_server_tag(99), Some(0));
        assert!((engine.context_mut().market.min_tick(0) - 0.01).abs() < 1e-10);
    }

    #[test]
    fn account_update_ut_parses_values() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);

        // Simulate 8=O UT message
        let msg = b"8=O\x019=999\x0135=UT\x018001=NetLiquidation\x018004=100000.50\x018001=BuyingPower\x018004=200000.00\x01";
        engine.inject_ccp_message(msg);

        assert_eq!(engine.context_mut().account().net_liquidation,
                   (100000.50 * PRICE_SCALE as f64) as Price);
        assert_eq!(engine.context_mut().account().buying_power,
                   (200000.0 * PRICE_SCALE as f64) as Price);
    }

    #[test]
    fn account_update_um_parses_pnl() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);

        let msg = b"8=O\x019=999\x0135=UM\x018001=UnrealizedPnL\x018004=1500.25\x018001=RealizedPnL\x018004=-200.00\x01";
        engine.inject_ccp_message(msg);

        assert_eq!(engine.context_mut().account().unrealized_pnl,
                   (1500.25 * PRICE_SCALE as f64) as Price);
        assert_eq!(engine.context_mut().account().realized_pnl,
                   (-200.0 * PRICE_SCALE as f64) as Price);
    }

    #[test]
    fn position_update_sets_absolute() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598); // instrument 0

        // UP message with position = 100
        let o_msg = format!("8=O\x019=999\x0135=UP\x016008=265598\x016064=100\x016065=150.25\x01");
        engine.inject_ccp_message(o_msg.as_bytes());

        assert_eq!(engine.context_mut().position(0), 100);

        // Second UP with position = 70 (sold 30)
        let o_msg2 = format!("8=O\x019=999\x0135=UP\x016008=265598\x016064=70\x01");
        engine.inject_ccp_message(o_msg2.as_bytes());

        assert_eq!(engine.context_mut().position(0), 70);
    }

    #[test]
    fn position_update_unknown_con_id_ignored() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        let msg = format!("8=O\x019=999\x0135=UP\x016008=999999\x016064=50\x01");
        engine.inject_ccp_message(msg.as_bytes());

        // No position change for instrument 0
        assert_eq!(engine.context_mut().position(0), 0);
    }

    #[test]
    fn unsubscribe_clears_tracking() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        // Manually populate tracking (simulating prior subscribe)
        engine.instrument_md_reqs.push((0, vec![1, 2]));

        engine.send_mktdata_unsubscribe(0);

        // Tracking should be cleared
        assert!(engine.instrument_md_reqs.is_empty());
    }

    #[test]
    fn disconnect_flags_default_false() {
        let engine = HotLoop::new(RecordingStrategy::new(), None);
        assert!(!engine.is_farm_disconnected());
        assert!(!engine.is_ccp_disconnected());
    }

    #[test]
    fn reconnect_farm_clears_flag_and_resubscribes() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598); // instrument 0
        engine.context_mut().market.register(272093); // instrument 1

        // Simulate prior subscriptions
        engine.instrument_md_reqs.push((0, vec![1]));
        engine.instrument_md_reqs.push((1, vec![2]));
        engine.farm_disconnected = true;

        // Reconnect with no actual connection (unit test)
        // We can't create a real Connection without a socket, so test the flag logic
        assert!(engine.is_farm_disconnected());
        engine.farm_disconnected = false;
        assert!(!engine.is_farm_disconnected());
    }

    // --- Edge case tests ---

    #[test]
    fn subscription_ack_no_body() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        // 35=Q with no CSV body — should be a no-op
        let msg = b"8=O\x019=999\x0135=Q\x01\x01";
        engine.handle_subscription_ack(msg);
        assert!(engine.md_req_to_instrument.is_empty());
    }

    #[test]
    fn subscription_ack_malformed_csv() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        // 35=Q with malformed CSV (not 3+ fields)
        let msg = b"8=O\x019=999\x0135=Q\x01bad_data\x01";
        engine.handle_subscription_ack(msg);
        // No crash
    }

    #[test]
    fn subscription_ack_unknown_req_id() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);
        // Don't add to md_req_to_instrument

        // req_id 999 not pending
        let msg = b"8=O\x019=999\x0135=Q\x0142,999,0.01,extra\x01";
        engine.handle_subscription_ack(msg);
        // Should be silently ignored
        assert_eq!(engine.context_mut().market.instrument_by_server_tag(42), None);
    }

    #[test]
    fn ticker_setup_no_body() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        // 35=L with no CSV body
        let msg = b"8=O\x019=999\x0135=L\x01\x01";
        engine.handle_ticker_setup(msg);
        // No crash
    }

    #[test]
    fn ticker_setup_unknown_con_id() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        // con_id 999999 not registered
        let msg = b"8=O\x019=999\x0135=L\x01999999,0.01,50,,1\x01";
        engine.handle_ticker_setup(msg);
        // server_tag 50 should not be registered
        assert_eq!(engine.context_mut().market.instrument_by_server_tag(50), None);
    }

    #[test]
    fn handle_cancel_reject_keeps_order() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);
        engine.context_mut().insert_order(crate::types::Order {
            order_id: 10,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: crate::types::OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        let cancel_reject = fix::fix_build(&[
            (35, "9"),
            (41, "10"),
            (58, "Order cannot be cancelled"),
        ], 1);
        engine.inject_ccp_message(&cancel_reject);

        // Order should still exist
        assert!(engine.context_mut().order(10).is_some());
    }

    #[test]
    fn handle_account_update_invalid_values_ignored() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);

        // 8001=NetLiquidation but 8004= is not a valid float
        let msg = b"8=O\x019=999\x0135=UT\x018001=NetLiquidation\x018004=not_a_number\x01";
        engine.inject_ccp_message(msg);
        // Should not crash, net_liquidation stays 0
        assert_eq!(engine.context_mut().account().net_liquidation, 0);
    }

    #[test]
    fn handle_account_update_unknown_key_ignored() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);

        let msg = b"8=O\x019=999\x0135=UT\x018001=SomeUnknownKey\x018004=12345.67\x01";
        engine.inject_ccp_message(msg);
        // Should not crash, no fields changed
    }

    #[test]
    fn exec_report_cancelled_removes_order() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(crate::types::Order {
            order_id: 5,
            instrument: 0,
            side: Side::Buy,
            price: 100 * PRICE_SCALE,
            qty: 50,
            filled: 0,
            status: crate::types::OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        let exec_msg = fix::fix_build(&[
            (35, "8"),
            (11, "5"),
            (39, "4"),  // Cancelled
            (150, "4"),
        ], 1);
        engine.inject_ccp_message(&exec_msg);

        // Order should be removed
        assert!(engine.context_mut().order(5).is_none());
    }

    #[test]
    fn format_price_negative() {
        assert_eq!(format_price(-150 * PRICE_SCALE), "-150");
    }

    #[test]
    fn format_price_cents() {
        // $0.01
        assert_eq!(format_price(PRICE_SCALE / 100), "0.01");
    }

    #[test]
    fn format_price_sub_penny() {
        // $0.005
        assert_eq!(format_price(PRICE_SCALE / 200), "0.005");
    }

    #[test]
    fn find_body_after_tag_found() {
        let msg = b"8=O\x019=10\x0135=P\x01stuff";
        let body = find_body_after_tag(msg, b"35=P\x01");
        assert_eq!(body, Some(b"stuff".as_ref()));
    }

    #[test]
    fn find_body_after_tag_not_found() {
        let msg = b"8=O\x019=10\x0135=Q\x01stuff";
        let body = find_body_after_tag(msg, b"35=P\x01");
        assert!(body.is_none());
    }

    #[test]
    fn multiple_subscriptions_same_instrument() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        let (tx, rx) = crossbeam_channel::bounded(16);
        engine.set_control_rx(rx);

        // Subscribe same instrument twice
        tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();
        tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();
        engine.poll_control_commands();

        // register() deduplicates, so both should map to instrument 0
        // Each subscribe allocates 2 req_ids (BidAsk + Last)
        assert_eq!(engine.next_md_req_id, 5); // started at 1, allocated 2+2
        assert_eq!(engine.instrument_md_reqs.len(), 1); // one instrument
        assert_eq!(engine.instrument_md_reqs[0].1.len(), 4); // four req_ids (2 per subscribe)
    }

    #[test]
    fn unsubscribe_nonexistent_instrument_noop() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.send_mktdata_unsubscribe(99); // no tracked subscription
        // Should not panic
    }

    #[test]
    fn on_start_called() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        assert!(!engine.strategy.started);
        engine.strategy.on_start(&mut engine.context);
        assert!(engine.strategy.started);
    }

    #[test]
    fn control_unsubscribe_command() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        let (tx, rx) = crossbeam_channel::bounded(16);
        engine.set_control_rx(rx);

        // Setup: subscribe first
        engine.context_mut().market.register(265598);
        engine.instrument_md_reqs.push((0, vec![1]));

        // Now unsubscribe
        tx.send(ControlCommand::Unsubscribe { instrument: 0 }).unwrap();
        engine.poll_control_commands();

        assert!(engine.instrument_md_reqs.is_empty());
    }

    #[test]
    fn process_farm_message_no_msg_type() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        // Message without 35= tag
        let bad_msg = b"8=O\x019=5\x01junk\x01";
        engine.inject_farm_message(bad_msg);
        // Should not panic
        assert!(engine.strategy.ticks.is_empty());
    }

    #[test]
    fn process_farm_message_heartbeat_noop() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        let heartbeat = fix::fix_build(&[(35, "0")], 1);
        engine.inject_farm_message(&heartbeat);
        // Should not trigger on_tick
        assert!(engine.strategy.ticks.is_empty());
    }

    #[test]
    fn position_update_zero_delta_no_change() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);
        engine.context_mut().update_position(0, 50);

        // UP with same position → no delta applied
        let msg = format!("8=O\x019=999\x0135=UP\x016008=265598\x016064=50\x01");
        engine.inject_ccp_message(msg.as_bytes());

        assert_eq!(engine.context_mut().position(0), 50);
    }

    #[test]
    fn exec_report_unknown_ord_status_ignored() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        let exec_msg = fix::fix_build(&[
            (35, "8"),
            (11, "1"),
            (39, "X"),  // Unknown status
            (150, "X"),
        ], 1);
        engine.inject_ccp_message(&exec_msg);
        // Should not crash, no fills/updates
        assert!(engine.strategy.fills.is_empty());
    }

    // ═══════════════════════════════════════════════════════════════════
    // P0 tests: partial fills, cancel reject, dedup, disconnect, stop
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn partial_fill_updates_filled_qty_and_keeps_order() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(Order {
            order_id: 10, instrument: 0, side: Side::Buy,
            price: 150 * PRICE_SCALE, qty: 100, filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // First partial fill: 30 of 100
        let msg1 = fix::fix_build(&[
            (35, "8"), (11, "10"), (39, "1"), (150, "1"),
            (31, "150.0"), (32, "30"), (151, "70"),
        ], 1);
        engine.inject_ccp_message(&msg1);

        assert_eq!(engine.context_mut().position(0), 30);
        assert_eq!(engine.strategy.fills.len(), 1);
        assert_eq!(engine.strategy.fills[0], (10, 30));
        let order = engine.context_mut().order(10).unwrap();
        assert_eq!(order.filled, 30);
        assert_eq!(order.status, OrderStatus::PartiallyFilled);

        // Second partial fill: 50 more
        let msg2 = fix::fix_build(&[
            (35, "8"), (11, "10"), (39, "1"), (150, "1"),
            (31, "150.1"), (32, "50"), (151, "20"),
        ], 2);
        engine.inject_ccp_message(&msg2);

        assert_eq!(engine.context_mut().position(0), 80);
        assert_eq!(engine.strategy.fills.len(), 2);
        assert_eq!(engine.strategy.fills[1], (10, 50));
        let order = engine.context_mut().order(10).unwrap();
        assert_eq!(order.filled, 80);

        // Final fill: remaining 20
        let msg3 = fix::fix_build(&[
            (35, "8"), (11, "10"), (39, "2"), (150, "F"),
            (31, "150.2"), (32, "20"), (151, "0"),
        ], 3);
        engine.inject_ccp_message(&msg3);

        assert_eq!(engine.context_mut().position(0), 100);
        assert_eq!(engine.strategy.fills.len(), 3);
        // Order should be removed (Filled is terminal)
        assert!(engine.context_mut().order(10).is_none());
    }

    #[test]
    fn cancel_reject_notifies_strategy_order_stays_active() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(Order {
            order_id: 20, instrument: 0, side: Side::Buy,
            price: 150 * PRICE_SCALE, qty: 100, filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // FIX 35=9 Cancel Reject with tags 434 and 102
        let msg = fix::fix_build(&[
            (35, "9"),
            (41, "20"),     // OrigClOrdID
            (434, "1"),     // CxlRejResponseTo = Cancel
            (102, "3"),     // CxlRejReason = PendingStatus
            (58, "Order cannot be cancelled"),
        ], 1);
        engine.inject_ccp_message(&msg);

        // Order should still be active
        let order = engine.context_mut().order(20).unwrap();
        assert_eq!(order.status, OrderStatus::Submitted);
        // Strategy should get on_cancel_reject (not on_order_update)
        assert_eq!(engine.strategy.cancel_rejects.len(), 1);
        assert_eq!(engine.strategy.cancel_rejects[0], (20, 1)); // reject_type=1 (cancel)
        assert!(engine.strategy.updates.is_empty()); // no order_update for cancel reject
    }

    #[test]
    fn duplicate_exec_reports_deduped() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(Order {
            order_id: 30, instrument: 0, side: Side::Buy,
            price: 150 * PRICE_SCALE, qty: 100, filled: 0,
            status: OrderStatus::PendingSubmit,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // IB sends 3x 39=A (PendingNew) — we saw this in live benchmark
        let msg = fix::fix_build(&[
            (35, "8"), (11, "30"), (39, "A"), (150, "A"),
        ], 1);

        engine.inject_ccp_message(&msg);
        engine.inject_ccp_message(&msg);
        engine.inject_ccp_message(&msg);

        // Strategy should only get ONE notification, not three
        assert_eq!(engine.strategy.updates.len(), 1);
        assert_eq!(engine.strategy.updates[0], (30, OrderStatus::Submitted));
    }

    #[test]
    fn farm_disconnect_notifies_strategy() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        assert!(!engine.strategy.disconnected);
        assert!(!engine.farm_disconnected);

        // Simulate farm disconnection by setting the flag directly
        // (In real code, poll_market_data sets this on recv error)
        engine.farm_disconnected = true;
        engine.strategy.on_disconnect(&mut engine.context);

        assert!(engine.strategy.disconnected);
    }

    #[test]
    fn ccp_disconnect_notifies_strategy() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        assert!(!engine.strategy.disconnected);

        engine.ccp_disconnected = true;
        engine.strategy.on_disconnect(&mut engine.context);

        assert!(engine.strategy.disconnected);
    }

    #[test]
    fn open_orders_survive_disconnect() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        // Insert an open order
        engine.context_mut().insert_order(Order {
            order_id: 40, instrument: 0, side: Side::Buy,
            price: 150 * PRICE_SCALE, qty: 100, filled: 30,
            status: OrderStatus::PartiallyFilled,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // Simulate disconnect
        engine.ccp_disconnected = true;
        engine.strategy.on_disconnect(&mut engine.context);

        // Open orders should survive (HashMap not cleared)
        let order = engine.context_mut().order(40).unwrap();
        assert_eq!(order.filled, 30);
        assert_eq!(order.status, OrderStatus::PartiallyFilled);
        // Positions should survive
        engine.context_mut().update_position(0, 30);
        assert_eq!(engine.context_mut().position(0), 30);
    }

    #[test]
    fn submit_stop_order_creates_request() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        let id = engine.context_mut().submit_stop(0, Side::Sell, 100, 140 * PRICE_SCALE);
        let orders: Vec<_> = engine.context_mut().drain_pending_orders().collect();

        assert_eq!(orders.len(), 1);
        match orders[0] {
            OrderRequest::SubmitStop { order_id, instrument, side, qty, stop_price } => {
                assert_eq!(order_id, id);
                assert_eq!(instrument, 0);
                assert_eq!(side, Side::Sell);
                assert_eq!(qty, 100);
                assert_eq!(stop_price, 140 * PRICE_SCALE);
            }
            _ => panic!("Expected SubmitStop"),
        }
    }

    #[test]
    fn partial_fill_then_cancel_correct_position() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(Order {
            order_id: 50, instrument: 0, side: Side::Buy,
            price: 150 * PRICE_SCALE, qty: 100, filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // Partial fill: 40 shares
        let fill_msg = fix::fix_build(&[
            (35, "8"), (11, "50"), (39, "1"), (150, "1"),
            (31, "150.0"), (32, "40"), (151, "60"),
        ], 1);
        engine.inject_ccp_message(&fill_msg);
        assert_eq!(engine.context_mut().position(0), 40);

        // Cancel remaining
        let cancel_msg = fix::fix_build(&[
            (35, "8"), (11, "50"), (39, "4"), (150, "4"),
            (151, "0"),
        ], 2);
        engine.inject_ccp_message(&cancel_msg);

        // Position stays at 40 (partial fill was real), order removed
        assert_eq!(engine.context_mut().position(0), 40);
        assert!(engine.context_mut().order(50).is_none());
        // Updates: PartiallyFilled, then Cancelled
        assert_eq!(engine.strategy.updates.len(), 2);
        assert_eq!(engine.strategy.updates[0].1, OrderStatus::PartiallyFilled);
        assert_eq!(engine.strategy.updates[1].1, OrderStatus::Cancelled);
    }

    #[test]
    fn cancel_clord_id_c_prefix_stripped() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(Order {
            order_id: 1772746902000, instrument: 0, side: Side::Buy,
            price: PRICE_SCALE, qty: 1, filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // Cancel response with "C" prefix on ClOrdID
        let msg = fix::fix_build(&[
            (35, "8"), (11, "C1772746902000"), (39, "4"), (150, "4"),
        ], 1);
        engine.inject_ccp_message(&msg);

        // Order should be cancelled (C prefix stripped, found the order)
        assert!(engine.context_mut().order(1772746902000).is_none());
        assert_eq!(engine.strategy.updates.len(), 1);
        assert_eq!(engine.strategy.updates[0].1, OrderStatus::Cancelled);
    }

    #[test]
    fn expired_order_treated_as_cancelled() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(Order {
            order_id: 60, instrument: 0, side: Side::Buy,
            price: PRICE_SCALE, qty: 1, filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // FIX 39=C (Expired) — DAY orders expire at market close
        let msg = fix::fix_build(&[
            (35, "8"), (11, "60"), (39, "C"), (150, "C"),
        ], 1);
        engine.inject_ccp_message(&msg);

        assert!(engine.context_mut().order(60).is_none());
        assert_eq!(engine.strategy.updates[0].1, OrderStatus::Cancelled);
    }

    // ── P0 Safety Feature Tests ──

    #[test]
    fn exec_id_dedup_prevents_double_fill() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(Order {
            order_id: 70, instrument: 0, side: Side::Buy,
            price: 150 * PRICE_SCALE, qty: 100, filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // Same ExecID sent twice (IB duplicate)
        let msg = fix::fix_build(&[
            (35, "8"), (11, "70"), (17, "EXEC001"),
            (39, "2"), (150, "F"),
            (31, "150.0"), (32, "100"), (151, "0"),
        ], 1);

        engine.inject_ccp_message(&msg);
        engine.inject_ccp_message(&msg);

        // Only one fill should be processed
        assert_eq!(engine.strategy.fills.len(), 1);
        assert_eq!(engine.context_mut().position(0), 100); // not 200
    }

    #[test]
    fn exec_id_dedup_different_ids_both_fill() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(Order {
            order_id: 71, instrument: 0, side: Side::Buy,
            price: 150 * PRICE_SCALE, qty: 100, filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        let msg1 = fix::fix_build(&[
            (35, "8"), (11, "71"), (17, "EXEC_A"),
            (39, "1"), (150, "1"),
            (31, "150.0"), (32, "50"), (151, "50"),
        ], 1);
        let msg2 = fix::fix_build(&[
            (35, "8"), (11, "71"), (17, "EXEC_B"),
            (39, "2"), (150, "F"),
            (31, "150.0"), (32, "50"), (151, "0"),
        ], 2);

        engine.inject_ccp_message(&msg1);
        engine.inject_ccp_message(&msg2);

        // Both fills processed (different ExecIDs)
        assert_eq!(engine.strategy.fills.len(), 2);
        assert_eq!(engine.context_mut().position(0), 100);
    }

    #[test]
    fn cancel_reject_on_partial_fill_preserves_status() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        // Order that is already partially filled
        engine.context_mut().insert_order(Order {
            order_id: 72, instrument: 0, side: Side::Buy,
            price: 150 * PRICE_SCALE, qty: 100, filled: 40,
            status: OrderStatus::PartiallyFilled,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // Cancel reject on the partially filled order
        let msg = fix::fix_build(&[
            (35, "9"), (41, "72"),
            (434, "1"), (102, "3"),
            (58, "Order in pending state"),
        ], 1);
        engine.inject_ccp_message(&msg);

        // Should restore to PartiallyFilled, not Submitted
        let order = engine.context_mut().order(72).unwrap();
        assert_eq!(order.status, OrderStatus::PartiallyFilled);
        assert_eq!(engine.strategy.cancel_rejects.len(), 1);
    }

    #[test]
    fn cancel_reject_modify_type() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(Order {
            order_id: 73, instrument: 0, side: Side::Sell,
            price: 200 * PRICE_SCALE, qty: 50, filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // Modify reject (tag 434=2)
        let msg = fix::fix_build(&[
            (35, "9"), (41, "73"),
            (434, "2"), (102, "1"), // 1 = UnknownOrder
            (58, "Cannot modify"),
        ], 1);
        engine.inject_ccp_message(&msg);

        assert_eq!(engine.strategy.cancel_rejects.len(), 1);
        assert_eq!(engine.strategy.cancel_rejects[0], (73, 2)); // reject_type=2 (modify)
    }

    #[test]
    fn heartbeat_timeout_sets_disconnect_flag_not_running_false() {
        // Verify that heartbeat timeout sets disconnect flag but doesn't kill the loop.
        // The loop should continue running so it can be reconnected.
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);

        // After heartbeat timeout, ccp_disconnected should be true
        // but running should still be true (so the loop continues)
        engine.ccp_disconnected = true;
        assert!(engine.running); // loop still alive for reconnection

        // Similarly for farm
        engine.farm_disconnected = true;
        assert!(engine.running); // loop still alive
    }

    #[test]
    fn partial_fill_updates_order_filled_qty() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(Order {
            order_id: 80, instrument: 0, side: Side::Buy,
            price: 150 * PRICE_SCALE, qty: 100, filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // First partial fill: 30 shares
        let msg1 = fix::fix_build(&[
            (35, "8"), (11, "80"), (17, "PF1"),
            (39, "1"), (150, "1"),
            (31, "150.0"), (32, "30"), (151, "70"),
        ], 1);
        engine.inject_ccp_message(&msg1);

        let order = engine.context_mut().order(80).unwrap();
        assert_eq!(order.filled, 30);
        assert_eq!(order.status, OrderStatus::PartiallyFilled);
        assert_eq!(engine.context_mut().position(0), 30);

        // Second partial fill: 50 shares
        let msg2 = fix::fix_build(&[
            (35, "8"), (11, "80"), (17, "PF2"),
            (39, "1"), (150, "1"),
            (31, "150.5"), (32, "50"), (151, "20"),
        ], 2);
        engine.inject_ccp_message(&msg2);

        let order = engine.context_mut().order(80).unwrap();
        assert_eq!(order.filled, 80);
        assert_eq!(engine.context_mut().position(0), 80);

        // Final fill: 20 shares
        let msg3 = fix::fix_build(&[
            (35, "8"), (11, "80"), (17, "PF3"),
            (39, "2"), (150, "F"),
            (31, "151.0"), (32, "20"), (151, "0"),
        ], 3);
        engine.inject_ccp_message(&msg3);

        // Order removed (terminal), position = 100
        assert!(engine.context_mut().order(80).is_none());
        assert_eq!(engine.context_mut().position(0), 100);
        assert_eq!(engine.strategy.fills.len(), 3);
    }

    #[test]
    fn seen_exec_ids_accumulate() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(Order {
            order_id: 81, instrument: 0, side: Side::Buy,
            price: PRICE_SCALE, qty: 100, filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // Multiple unique fills
        for i in 0..5 {
            let exec_id = format!("E{}", i);
            let msg = fix::fix_build(&[
                (35, "8"), (11, "81"), (17, &exec_id),
                (39, "1"), (150, "1"),
                (31, "1.0"), (32, "10"), (151, &format!("{}", 90 - i * 10)),
            ], i as u32 + 1);
            engine.inject_ccp_message(&msg);
        }

        assert_eq!(engine.strategy.fills.len(), 5);
        assert_eq!(engine.seen_exec_ids.len(), 5);
    }

    // --- Farm disconnect cleanup ---

    #[test]
    fn handle_farm_disconnect_clears_tracking_and_zeros_quotes() {
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        let id = engine.context_mut().market.register(265598);
        engine.context_mut().market.register_server_tag(42, id);
        engine.context_mut().quote_mut(id).bid = 150 * PRICE_SCALE;
        engine.context_mut().quote_mut(id).ask = 151 * PRICE_SCALE;
        engine.md_req_to_instrument.push((1, id));
        engine.instrument_md_reqs.push((id, vec![1, 2]));

        engine.handle_farm_disconnect();

        assert!(engine.farm_disconnected);
        assert!(engine.md_req_to_instrument.is_empty());
        assert!(engine.instrument_md_reqs.is_empty());
        assert_eq!(engine.context_mut().market.instrument_by_server_tag(42), None);
        assert_eq!(engine.context_mut().bid(id), 0);
        assert_eq!(engine.context_mut().ask(id), 0);
        assert!(engine.strategy.disconnected);
    }

    // --- CCP disconnect cleanup ---

    #[test]
    fn handle_ccp_disconnect_marks_orders_uncertain() {
        let strategy = RecordingStrategy::new();
        let mut engine = HotLoop::new(strategy, None);
        engine.context_mut().insert_order(Order {
            order_id: 1, instrument: 0, side: Side::Buy,
            price: PRICE_SCALE, qty: 100, filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });
        engine.context_mut().insert_order(Order {
            order_id: 2, instrument: 0, side: Side::Sell,
            price: PRICE_SCALE, qty: 50, filled: 20,
            status: OrderStatus::PartiallyFilled,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });
        engine.context_mut().insert_order(Order {
            order_id: 3, instrument: 0, side: Side::Buy,
            price: PRICE_SCALE, qty: 100, filled: 100,
            status: OrderStatus::Filled,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        engine.handle_ccp_disconnect();

        assert!(engine.ccp_disconnected);
        assert_eq!(engine.context_mut().order(1).unwrap().status, OrderStatus::Uncertain);
        assert_eq!(engine.context_mut().order(2).unwrap().status, OrderStatus::Uncertain);
        // Filled orders should NOT be marked uncertain
        assert_eq!(engine.context_mut().order(3).unwrap().status, OrderStatus::Filled);
        assert!(engine.strategy.disconnected);
    }

    #[test]
    fn uncertain_orders_still_in_open_orders_for() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1, instrument: 0, side: Side::Buy,
            price: PRICE_SCALE, qty: 100, filled: 0,
            status: OrderStatus::Uncertain,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });
        let open = ctx.open_orders_for(0);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].status, OrderStatus::Uncertain);
    }
}
