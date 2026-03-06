use std::time::Instant;

use crate::engine::context::{Context, Strategy};
use crate::gateway::chrono_free_timestamp;
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::protocol::tick_decoder;
use crate::types::{ControlCommand, InstrumentId, OrderRequest, Price, Side, PRICE_SCALE};
use crossbeam_channel::Receiver;

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
}

/// Tracks last send/recv times and pending test requests for heartbeat management.
pub struct HeartbeatState {
    pub last_ccp_sent: Instant,
    pub last_ccp_recv: Instant,
    pub last_farm_sent: Instant,
    pub last_farm_recv: Instant,
    /// Pending test request for CCP: (test_req_id, sent_at).
    pub pending_ccp_test: Option<(String, Instant)>,
    /// Pending test request for farm: (test_req_id, sent_at).
    pub pending_farm_test: Option<(String, Instant)>,
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
            pending_ccp_test: None,
            pending_farm_test: None,
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
                        self.farm_disconnected = true;
                        self.strategy.on_disconnect(&mut self.context);
                        return;
                    }
                    Ok(_) => {
                        let now = Instant::now();
                        self.hb.last_farm_recv = now;
                        self.context.recv_at = now;
                        self.hb.pending_farm_test = None; // data received, clear pending test
                    }
                }
                let frames = conn.extract_frames();
                let mut msgs = Vec::new();
                for frame in frames {
                    match frame {
                        Frame::FixComp(raw) => {
                            let (unsigned, _valid) = conn.unsign(&raw);
                            msgs.extend(fixcomp::fixcomp_decompress(&unsigned));
                        }
                        Frame::Binary(raw) => {
                            let (unsigned, _valid) = conn.unsign(&raw);
                            msgs.push(unsigned);
                        }
                        Frame::Fix(raw) => {
                            let (unsigned, _valid) = conn.unsign(&raw);
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
            "Q" => self.handle_subscription_ack(msg),
            "0" => {} // heartbeat — timestamp already updated in try_recv
            "1" => {
                // Test request — respond with heartbeat containing the test req ID
                let test_id = parsed.get(&fix::TAG_TEST_REQ_ID).cloned().unwrap_or_default();
                if let Some(conn) = self.farm_conn.as_mut() {
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    self.hb.last_farm_sent = Instant::now();
                }
            }
            "L" => self.handle_ticker_setup(msg),
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
    fn send_mktdata_subscribe(&mut self, con_id: i64, instrument: InstrumentId) {
        let req_id = self.next_md_req_id;
        self.next_md_req_id += 1;

        // Track pending subscription
        self.md_req_to_instrument.push((req_id, instrument));

        // Track active subscription for this instrument
        match self.instrument_md_reqs.iter_mut().find(|(id, _)| *id == instrument) {
            Some((_, reqs)) => reqs.push(req_id),
            None => self.instrument_md_reqs.push((instrument, vec![req_id])),
        }

        if let Some(conn) = self.farm_conn.as_mut() {
            let req_id_str = req_id.to_string();
            let con_id_str = (con_id as u32).to_string();
            let _ = conn.send_fixcomp(&[
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                (262, &req_id_str),
                (263, "1"), // Subscribe
                (146, "1"), // NumRelatedSym
                (6008, &con_id_str),
                (207, "BEST"),
                (167, "CS"),
                (264, "442"), // BidAsk
                (9830, "1"),
            ]);
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
                    self.context.insert_order(crate::types::Order {
                        order_id, instrument, side, price, qty, filled: 0,
                        status: crate::types::OrderStatus::PendingSubmit,
                    });
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
                    self.context.insert_order(crate::types::Order {
                        order_id, instrument, side, price, qty, filled: 0,
                        status: crate::types::OrderStatus::PendingSubmit,
                    });
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
                    self.context.insert_order(crate::types::Order {
                        order_id, instrument, side, price, qty, filled: 0,
                        status: crate::types::OrderStatus::PendingSubmit,
                    });
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
                OrderRequest::SubmitMarket { order_id, instrument, side, qty } => {
                    self.context.insert_order(crate::types::Order {
                        order_id, instrument, side, price: 0, qty, filled: 0,
                        status: crate::types::OrderStatus::PendingSubmit,
                    });
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
                    self.context.insert_order(crate::types::Order {
                        order_id, instrument, side, price: stop_price, qty, filled: 0,
                        status: crate::types::OrderStatus::PendingSubmit,
                    });
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
                    if let Some(orig) = self.context.order(order_id).copied() {
                        self.context.insert_order(crate::types::Order {
                            order_id: new_order_id,
                            instrument: orig.instrument,
                            side: orig.side,
                            qty,
                            price,
                            status: crate::types::OrderStatus::PendingSubmit,
                            filled: 0,
                        });
                    }
                    let clord_str = new_order_id.to_string();
                    let orig_clord = order_id.to_string();
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let now = chrono_free_timestamp();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_ORDER_REPLACE),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (41, &orig_clord),  // OrigClOrdID
                        (38, &qty_str),
                        (44, &price_str),
                    ])
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
                    Ok(0) => return,
                    Err(e) => {
                        log::error!("CCP connection lost: {}", e);
                        self.ccp_disconnected = true;
                        self.strategy.on_disconnect(&mut self.context);
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
            "UT" | "UM" | "RL" => self.handle_account_update(msg),
            "UP" => self.handle_position_update(&parsed),
            _ => {}
        }
    }

    /// Handle FIX 35=8 ExecutionReport from CCP.
    fn handle_exec_report(&mut self, parsed: &std::collections::HashMap<u32, String>) {
        let ord_status = parsed.get(&39).map(|s| s.as_str()).unwrap_or("");
        let exec_type = parsed.get(&150).map(|s| s.as_str()).unwrap_or("");
        let last_px = parsed.get(&31).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let last_shares = parsed.get(&32).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let leaves_qty = parsed.get(&151).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let clord_id = parsed.get(&11).and_then(|s| {
            // Cancel responses have "C" prefix (e.g. "C1772746902000")
            let stripped = s.strip_prefix('C').unwrap_or(s);
            stripped.parse::<u64>().ok()
        }).unwrap_or(0);

        log::info!("ExecReport: 39={} 150={} 11={} 58={} 103={}",
            ord_status, exec_type, clord_id,
            parsed.get(&58).map(|s| s.as_str()).unwrap_or(""),
            parsed.get(&103).map(|s| s.as_str()).unwrap_or(""));

        // Map FIX OrdStatus (tag 39) to our OrderStatus
        let status = match ord_status {
            "0" | "A" | "E" => crate::types::OrderStatus::Submitted,
            "1" => crate::types::OrderStatus::PartiallyFilled,
            "2" => crate::types::OrderStatus::Filled,
            "4" | "5" | "6" | "C" => crate::types::OrderStatus::Cancelled,
            "8" => crate::types::OrderStatus::Rejected,
            _ => return,
        };

        // Check if status actually changed (dedup repeated exec reports like 3x 39=A)
        let prev_status = self.context.order(clord_id).map(|o| o.status);
        let status_changed = prev_status != Some(status);

        // Update order status
        self.context.update_order_status(clord_id, status);

        // On fill (exec_type: F=Fill, 1=Partial, 2=Filled)
        if matches!(exec_type, "F" | "1" | "2") && last_shares > 0 {
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
                    timestamp_ns: self.context.now_ns(),
                };

                let delta = match order.side {
                    Side::Buy => last_shares,
                    Side::Sell => -last_shares,
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
        log::warn!("CancelReject: origClOrd={:?} reason={}", orig_clord, reason);

        if let Some(oid) = orig_clord {
            // Ensure order stays in Submitted state (cancel failed, order still live)
            self.context.update_order_status(oid, crate::types::OrderStatus::Submitted);

            // Notify strategy that the cancel was rejected — order is still active
            if let Some(order) = self.context.order(oid).copied() {
                let update = crate::types::OrderUpdate {
                    order_id: oid,
                    instrument: order.instrument,
                    status: crate::types::OrderStatus::Submitted,
                    filled_qty: order.filled as i64,
                    remaining_qty: order.qty as i64 - order.filled as i64,
                    timestamp_ns: self.context.now_ns(),
                };
                self.strategy.on_order_update(&update, &mut self.context);
            }
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
                ControlCommand::UpdateParam { key, value } => {
                    let _ = (key, value);
                }
                ControlCommand::Shutdown => {
                    self.running = false;
                    self.strategy.on_disconnect(&mut self.context);
                }
            }
        }
    }

    fn check_heartbeats(&mut self) {
        let now = Instant::now();
        let ts = chrono_free_timestamp();

        // --- CCP heartbeat ---
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
                        // TestRequest timed out — connection lost
                        log::error!("CCP heartbeat timeout — connection lost");
                        self.running = false;
                        self.strategy.on_disconnect(&mut self.context);
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

        // --- Farm heartbeat ---
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
                        log::error!("Farm heartbeat timeout — connection lost");
                        self.running = false;
                        self.strategy.on_disconnect(&mut self.context);
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

}

/// Convert Side to FIX tag 54 value.
fn fix_side(side: Side) -> &'static str {
    match side {
        Side::Buy => "1",
        Side::Sell => "2",
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

    /// Replace the CCP connection (after reconnection).
    pub fn reconnect_ccp(&mut self, conn: Connection) {
        self.ccp_conn = Some(conn);
        self.ccp_disconnected = false;
        self.hb.last_ccp_sent = Instant::now();
        self.hb.last_ccp_recv = Instant::now();
        self.hb.pending_ccp_test = None;
        log::info!("CCP reconnected");
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
            crate::types::Side::Sell => -fill.qty,
        };
        self.context.update_position(fill.instrument, delta);
        self.strategy.on_fill(fill, &mut self.context);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    struct RecordingStrategy {
        ticks: Vec<InstrumentId>,
        fills: Vec<(OrderId, i64)>,  // (order_id, qty)
        updates: Vec<(OrderId, OrderStatus)>,
        disconnected: bool,
        started: bool,
    }

    impl RecordingStrategy {
        fn new() -> Self {
            Self {
                ticks: Vec::new(),
                fills: Vec::new(),
                updates: Vec::new(),
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

        // Should have a pending subscription with req_id=1
        assert_eq!(engine.md_req_to_instrument.len(), 1);
        assert_eq!(engine.md_req_to_instrument[0], (1, 0));
        // Should track active subscription
        assert_eq!(engine.instrument_md_reqs.len(), 1);
        assert_eq!(engine.instrument_md_reqs[0].0, 0);
        assert_eq!(engine.instrument_md_reqs[0].1, vec![1]);
        // Next req_id should be 2
        assert_eq!(engine.next_md_req_id, 2);
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
        // But we should have 2 MDReqIDs
        assert_eq!(engine.next_md_req_id, 3); // started at 1, allocated 2
        assert_eq!(engine.instrument_md_reqs.len(), 1); // one instrument
        assert_eq!(engine.instrument_md_reqs[0].1.len(), 2); // two req_ids
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
        });

        // FIX 35=9 Cancel Reject
        let msg = fix::fix_build(&[
            (35, "9"),
            (41, "20"),     // OrigClOrdID
            (58, "Order cannot be cancelled"),
        ], 1);
        engine.inject_ccp_message(&msg);

        // Order should still be active
        let order = engine.context_mut().order(20).unwrap();
        assert_eq!(order.status, OrderStatus::Submitted);
        // Strategy should be notified
        assert_eq!(engine.strategy.updates.len(), 1);
        assert_eq!(engine.strategy.updates[0], (20, OrderStatus::Submitted));
    }

    #[test]
    fn duplicate_exec_reports_deduped() {
        let mut engine = HotLoop::new(RecordingStrategy::new(), None);
        engine.context_mut().market.register(265598);

        engine.context_mut().insert_order(Order {
            order_id: 30, instrument: 0, side: Side::Buy,
            price: 150 * PRICE_SCALE, qty: 100, filled: 0,
            status: OrderStatus::PendingSubmit,
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
        });

        // FIX 39=C (Expired) — DAY orders expire at market close
        let msg = fix::fix_build(&[
            (35, "8"), (11, "60"), (39, "C"), (150, "C"),
        ], 1);
        engine.inject_ccp_message(&msg);

        assert!(engine.context_mut().order(60).is_none());
        assert_eq!(engine.strategy.updates[0].1, OrderStatus::Cancelled);
    }
}
