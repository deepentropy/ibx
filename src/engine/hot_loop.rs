use crate::engine::context::{Context, Strategy};
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::protocol::tick_decoder;
use crate::types::{InstrumentId, OrderRequest, Price, Side, PRICE_SCALE};

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
    /// Next client order ID for FIX tag 11.
    next_clord_id: u64,
}

impl<S: Strategy> HotLoop<S> {
    pub fn new(strategy: S, core_id: Option<usize>) -> Self {
        Self {
            strategy,
            context: Context::new(),
            core_id,
            farm_conn: None,
            ccp_conn: None,
            next_clord_id: 1,
        }
    }

    /// Access the context (for pre-start configuration like registering instruments).
    pub fn context_mut(&mut self) -> &mut Context {
        &mut self.context
    }

    /// Run the hot loop. This blocks the current thread forever.
    pub fn run(&mut self) -> ! {
        if let Some(core) = self.core_id {
            Self::pin_to_core(core);
        }

        self.strategy.on_start(&mut self.context);

        loop {
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
        // Collect all messages from farm connection first, then process.
        // This avoids borrow conflicts between conn and self.
        let messages = match self.farm_conn.as_mut() {
            None => return,
            Some(conn) => {
                match conn.try_recv() {
                    Ok(0) => return,  // WouldBlock
                    Err(_) => return, // Connection error
                    Ok(_) => {}
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
            "Q" => self.handle_subscription_ack(&parsed),
            "0" => {} // heartbeat — no-op, timestamp updated in try_recv
            "1" => {} // test request — TODO: respond with heartbeat
            _ => {}   // other farm messages (35=L, 35=G, etc.)
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
    fn handle_subscription_ack(&mut self, parsed: &std::collections::HashMap<u32, String>) {
        // 35=Q body is CSV: serverTag,reqId,minTick,...
        // The body text is typically in a custom tag or the raw content
        // For now, we look for tag 262 (MDReqID) and tag 6040 fields
        // The actual parsing depends on the 8=O binary format
        // This will be refined when testing against real data
        let _ = parsed;
    }

    fn drain_and_send_orders(&mut self) {
        let orders: Vec<OrderRequest> = self.context.drain_pending_orders().collect();
        let conn = match self.ccp_conn.as_mut() {
            Some(c) => c,
            None => return,
        };
        for order_req in orders {
            let result = match order_req {
                OrderRequest::SubmitLimit { instrument, side, qty, price } => {
                    let clord = self.next_clord_id;
                    self.next_clord_id += 1;
                    let clord_str = clord.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (11, &clord_str),   // ClOrdID
                        (55, &instrument.to_string()), // Symbol (instrument ID as placeholder)
                        (54, side_str),     // Side
                        (38, &qty_str),     // OrderQty
                        (40, "2"),          // OrdType = Limit
                        (44, &price_str),   // Price
                        (59, "0"),          // TIF = DAY
                    ])
                }
                OrderRequest::SubmitMarket { instrument, side, qty } => {
                    let clord = self.next_clord_id;
                    self.next_clord_id += 1;
                    let clord_str = clord.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                        (11, &clord_str),
                        (55, &instrument.to_string()),
                        (54, side_str),
                        (38, &qty_str),
                        (40, "1"),          // OrdType = Market
                        (59, "0"),          // TIF = DAY
                    ])
                }
                OrderRequest::Cancel { order_id } => {
                    let clord_str = format!("C{}", order_id);
                    let orig_clord = order_id.to_string();
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_ORDER_CANCEL),
                        (11, &clord_str),   // ClOrdID (cancel)
                        (41, &orig_clord),  // OrigClOrdID
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
                        last_result = conn.send_fix(&[
                            (fix::TAG_MSG_TYPE, fix::MSG_ORDER_CANCEL),
                            (11, &clord_str),
                            (41, &orig_clord),
                        ]);
                    }
                    last_result
                }
                OrderRequest::Modify { order_id, price, qty } => {
                    let clord = self.next_clord_id;
                    self.next_clord_id += 1;
                    let clord_str = clord.to_string();
                    let orig_clord = order_id.to_string();
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_ORDER_REPLACE),
                        (11, &clord_str),
                        (41, &orig_clord),  // OrigClOrdID
                        (38, &qty_str),
                        (44, &price_str),
                    ])
                }
            };
            if let Err(e) = result {
                log::error!("Failed to send order: {}", e);
            }
        }
    }

    fn poll_executions(&mut self) {
        // TODO: Non-blocking recv from CCP socket
        // When execution report arrives:
        //   1. Decode FIX message
        //   2. Update self.context positions/orders
        //   3. Call self.strategy.on_fill() or on_order_update()
    }

    fn poll_control_commands(&mut self) {
        // TODO: Check SPSC ring buffer from control plane
        // Commands: subscribe instrument, change params, shutdown, etc.
    }

    fn check_heartbeats(&mut self) {
        // TODO: Track last send/recv times
        // CCP heartbeat: every 10s
        // Farm heartbeat: every 30s
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

    /// Inject a raw farm message for testing. Processes it through the full decode pipeline.
    pub fn inject_farm_message(&mut self, msg: &[u8]) {
        self.process_farm_message(msg);
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
        fills: Vec<OrderId>,
        started: bool,
    }

    impl RecordingStrategy {
        fn new() -> Self {
            Self {
                ticks: Vec::new(),
                fills: Vec::new(),
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
            self.fills.push(fill.order_id);
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
        assert_eq!(engine.strategy.fills, vec![1]);
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
}
