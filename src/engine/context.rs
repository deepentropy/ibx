use crate::engine::market_state::MarketState;
use crate::types::*;
use std::collections::HashMap;

/// The strategy trait. Generic type parameter S: Strategy is monomorphized into
/// the engine loop — no Box<dyn>, no vtable, no indirection.
pub trait Strategy {
    /// Called once after authentication and initial state sync.
    fn on_start(&mut self, ctx: &mut Context) {
        let _ = ctx;
    }

    /// Called on every market data tick. THE hot path.
    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context);

    /// Called when an order is filled (partial or full).
    fn on_fill(&mut self, fill: &Fill, ctx: &mut Context) {
        let _ = (fill, ctx);
    }

    /// Called on order status change (submitted, cancelled, rejected).
    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        let _ = (update, ctx);
    }

    /// Called on disconnect or error. Chance to cancel all orders.
    fn on_disconnect(&mut self, ctx: &mut Context) {
        let _ = ctx;
    }
}

/// TSC-calibrated clock for hot-path timestamps.
pub struct Clock {
    start: std::time::Instant,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }

    /// Monotonic nanoseconds since engine start. Fast, no syscall.
    #[inline(always)]
    pub fn now_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    /// Wall-clock Unix timestamp in seconds.
    pub fn now_utc(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }
}

/// The context passed to strategy callbacks. Provides market data access and
/// order management. All hot-path data is pre-allocated.
pub struct Context {
    pub(crate) market: MarketState,
    positions: [i64; MAX_INSTRUMENTS],
    open_orders: HashMap<OrderId, Order>,
    pending_orders: OrderBuffer,
    pub(crate) account: AccountState,
    clock: Clock,
    next_order_id: OrderId,
}

impl Context {
    pub fn new() -> Self {
        Self {
            market: MarketState::new(),
            positions: [0i64; MAX_INSTRUMENTS],
            open_orders: HashMap::with_capacity(128),
            pending_orders: OrderBuffer::new(),
            account: AccountState::default(),
            clock: Clock::new(),
            next_order_id: 1,
        }
    }

    // ── Market data (read, zero-copy) ──

    #[inline(always)]
    pub fn bid(&self, id: InstrumentId) -> Price {
        self.market.bid(id)
    }

    #[inline(always)]
    pub fn ask(&self, id: InstrumentId) -> Price {
        self.market.ask(id)
    }

    #[inline(always)]
    pub fn last(&self, id: InstrumentId) -> Price {
        self.market.last(id)
    }

    #[inline(always)]
    pub fn bid_size(&self, id: InstrumentId) -> Qty {
        self.market.bid_size(id)
    }

    #[inline(always)]
    pub fn ask_size(&self, id: InstrumentId) -> Qty {
        self.market.ask_size(id)
    }

    #[inline(always)]
    pub fn mid(&self, id: InstrumentId) -> Price {
        self.market.mid(id)
    }

    #[inline(always)]
    pub fn spread(&self, id: InstrumentId) -> Price {
        self.market.spread(id)
    }

    #[inline(always)]
    pub fn quote(&self, id: InstrumentId) -> &Quote {
        self.market.quote(id)
    }

    // ── Positions & orders (read) ──

    #[inline(always)]
    pub fn position(&self, id: InstrumentId) -> i64 {
        self.positions[id as usize]
    }

    pub fn open_orders_for(&self, id: InstrumentId) -> Vec<&Order> {
        self.open_orders
            .values()
            .filter(|o| o.instrument == id && o.status == OrderStatus::Submitted)
            .collect()
    }

    pub fn order(&self, order_id: OrderId) -> Option<&Order> {
        self.open_orders.get(&order_id)
    }

    pub fn account(&self) -> &AccountState {
        &self.account
    }

    // ── Order management (write to pre-allocated buffer) ──

    pub fn submit_limit(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimit {
            instrument,
            side,
            qty,
            price,
        });
        id
    }

    pub fn submit_market(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMarket {
            instrument,
            side,
            qty,
        });
        id
    }

    pub fn cancel(&mut self, order_id: OrderId) {
        self.pending_orders.push(OrderRequest::Cancel { order_id });
    }

    pub fn cancel_all(&mut self, instrument: InstrumentId) {
        self.pending_orders
            .push(OrderRequest::CancelAll { instrument });
    }

    pub fn modify(&mut self, order_id: OrderId, price: Price, qty: u32) {
        self.pending_orders.push(OrderRequest::Modify {
            order_id,
            price,
            qty,
        });
    }

    // ── Timing ──

    #[inline(always)]
    pub fn now_ns(&self) -> u64 {
        self.clock.now_ns()
    }

    pub fn now_utc(&self) -> i64 {
        self.clock.now_utc()
    }

    // ── Instrument management ──

    pub fn register_instrument(&mut self, con_id: i64) -> InstrumentId {
        self.market.register(con_id)
    }

    pub fn set_quote(&mut self, id: InstrumentId, quote: Quote) {
        *self.market.quote_mut(id) = quote;
    }

    pub fn quote_mut(&mut self, id: InstrumentId) -> &mut Quote {
        self.market.quote_mut(id)
    }

    // ── Engine-internal methods ──

    pub fn drain_pending_orders(&mut self) -> std::vec::Drain<'_, OrderRequest> {
        self.pending_orders.drain()
    }

    pub fn update_position(&mut self, instrument: InstrumentId, delta: i64) {
        self.positions[instrument as usize] += delta;
    }

    pub fn insert_order(&mut self, order: Order) {
        self.open_orders.insert(order.order_id, order);
    }

    pub fn update_order_status(&mut self, order_id: OrderId, status: OrderStatus) {
        if let Some(order) = self.open_orders.get_mut(&order_id) {
            order.status = status;
        }
    }

    pub fn remove_order(&mut self, order_id: OrderId) {
        self.open_orders.remove(&order_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Order submission & drain ---

    #[test]
    fn submit_limit_returns_incrementing_ids() {
        let mut ctx = Context::new();
        let id1 = ctx.submit_limit(0, Side::Buy, 100, 150 * PRICE_SCALE);
        let id2 = ctx.submit_limit(0, Side::Sell, 50, 151 * PRICE_SCALE);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn submit_limit_drains_correctly() {
        let mut ctx = Context::new();
        ctx.submit_limit(0, Side::Buy, 100, 150 * PRICE_SCALE);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match orders[0] {
            OrderRequest::SubmitLimit {
                instrument,
                side,
                qty,
                price,
            } => {
                assert_eq!(instrument, 0);
                assert_eq!(side, Side::Buy);
                assert_eq!(qty, 100);
                assert_eq!(price, 150 * PRICE_SCALE);
            }
            _ => panic!("expected SubmitLimit"),
        }
    }

    #[test]
    fn submit_market_drains_correctly() {
        let mut ctx = Context::new();
        ctx.submit_market(1, Side::Sell, 200);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match orders[0] {
            OrderRequest::SubmitMarket {
                instrument,
                side,
                qty,
            } => {
                assert_eq!(instrument, 1);
                assert_eq!(side, Side::Sell);
                assert_eq!(qty, 200);
            }
            _ => panic!("expected SubmitMarket"),
        }
    }

    #[test]
    fn cancel_drains_correctly() {
        let mut ctx = Context::new();
        ctx.cancel(42);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        match orders[0] {
            OrderRequest::Cancel { order_id } => assert_eq!(order_id, 42),
            _ => panic!("expected Cancel"),
        }
    }

    #[test]
    fn cancel_all_drains_correctly() {
        let mut ctx = Context::new();
        ctx.cancel_all(5);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        match orders[0] {
            OrderRequest::CancelAll { instrument } => assert_eq!(instrument, 5),
            _ => panic!("expected CancelAll"),
        }
    }

    #[test]
    fn modify_drains_correctly() {
        let mut ctx = Context::new();
        ctx.modify(7, 200 * PRICE_SCALE, 50);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        match orders[0] {
            OrderRequest::Modify {
                order_id,
                price,
                qty,
            } => {
                assert_eq!(order_id, 7);
                assert_eq!(price, 200 * PRICE_SCALE);
                assert_eq!(qty, 50);
            }
            _ => panic!("expected Modify"),
        }
    }

    #[test]
    fn drain_clears_buffer() {
        let mut ctx = Context::new();
        ctx.submit_limit(0, Side::Buy, 100, 150 * PRICE_SCALE);
        let _: Vec<_> = ctx.drain_pending_orders().collect();
        // Second drain should be empty
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert!(orders.is_empty());
    }

    #[test]
    fn multiple_orders_per_tick() {
        let mut ctx = Context::new();
        ctx.submit_limit(0, Side::Buy, 100, 150 * PRICE_SCALE);
        ctx.submit_limit(0, Side::Sell, 50, 152 * PRICE_SCALE);
        ctx.cancel(99);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 3);
    }

    // --- Position tracking ---

    #[test]
    fn position_starts_at_zero() {
        let ctx = Context::new();
        assert_eq!(ctx.position(0), 0);
        assert_eq!(ctx.position(255), 0);
    }

    #[test]
    fn update_position_accumulates() {
        let mut ctx = Context::new();
        ctx.update_position(0, 100);
        assert_eq!(ctx.position(0), 100);
        ctx.update_position(0, -30);
        assert_eq!(ctx.position(0), 70);
        ctx.update_position(0, -70);
        assert_eq!(ctx.position(0), 0);
    }

    #[test]
    fn positions_per_instrument() {
        let mut ctx = Context::new();
        ctx.update_position(0, 100);
        ctx.update_position(1, -50);
        assert_eq!(ctx.position(0), 100);
        assert_eq!(ctx.position(1), -50);
    }

    // --- Open orders ---

    #[test]
    fn insert_and_query_order() {
        let mut ctx = Context::new();
        let order = Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: OrderStatus::Submitted,
        };
        ctx.insert_order(order);
        assert!(ctx.order(1).is_some());
        assert_eq!(ctx.order(1).unwrap().qty, 100);
    }

    #[test]
    fn open_orders_for_instrument() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: OrderStatus::Submitted,
        });
        ctx.insert_order(Order {
            order_id: 2,
            instrument: 1,
            side: Side::Sell,
            price: 400 * PRICE_SCALE,
            qty: 50,
            filled: 0,
            status: OrderStatus::Submitted,
        });

        let inst0_orders = ctx.open_orders_for(0);
        assert_eq!(inst0_orders.len(), 1);
        assert_eq!(inst0_orders[0].order_id, 1);
    }

    #[test]
    fn update_order_status() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: OrderStatus::Submitted,
        });
        ctx.update_order_status(1, OrderStatus::Cancelled);
        assert_eq!(ctx.order(1).unwrap().status, OrderStatus::Cancelled);

        // Cancelled orders not in open_orders_for (filters by Submitted)
        assert!(ctx.open_orders_for(0).is_empty());
    }

    #[test]
    fn remove_order() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: OrderStatus::Submitted,
        });
        ctx.remove_order(1);
        assert!(ctx.order(1).is_none());
    }

    // --- Market data through context ---

    #[test]
    fn context_market_data_accessors() {
        let mut ctx = Context::new();
        let id = ctx.market.register(265598);
        let q = ctx.market.quote_mut(id);
        q.bid = 15000 * (PRICE_SCALE / 100);
        q.ask = 15010 * (PRICE_SCALE / 100);

        assert_eq!(ctx.bid(id), 15000 * (PRICE_SCALE / 100));
        assert_eq!(ctx.ask(id), 15010 * (PRICE_SCALE / 100));
        assert_eq!(ctx.spread(id), 10 * (PRICE_SCALE / 100));
        assert_eq!(ctx.mid(id), 15005 * (PRICE_SCALE / 100));
    }

    // --- Clock ---

    #[test]
    fn clock_monotonic() {
        let ctx = Context::new();
        let t1 = ctx.now_ns();
        let t2 = ctx.now_ns();
        assert!(t2 >= t1);
    }

    #[test]
    fn clock_utc_reasonable() {
        let ctx = Context::new();
        let ts = ctx.now_utc();
        // Should be after 2025-01-01 (1735689600)
        assert!(ts > 1_735_689_600);
    }

    // --- Strategy trait ---

    struct CountingStrategy {
        tick_count: u32,
        fill_count: u32,
        started: bool,
        disconnected: bool,
    }

    impl CountingStrategy {
        fn new() -> Self {
            Self {
                tick_count: 0,
                fill_count: 0,
                started: false,
                disconnected: false,
            }
        }
    }

    impl Strategy for CountingStrategy {
        fn on_start(&mut self, _ctx: &mut Context) {
            self.started = true;
        }

        fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {
            self.tick_count += 1;
        }

        fn on_fill(&mut self, _fill: &Fill, _ctx: &mut Context) {
            self.fill_count += 1;
        }

        fn on_disconnect(&mut self, _ctx: &mut Context) {
            self.disconnected = true;
        }
    }

    #[test]
    fn strategy_on_start() {
        let mut s = CountingStrategy::new();
        let mut ctx = Context::new();
        s.on_start(&mut ctx);
        assert!(s.started);
    }

    #[test]
    fn strategy_on_tick() {
        let mut s = CountingStrategy::new();
        let mut ctx = Context::new();
        s.on_tick(0, &mut ctx);
        s.on_tick(0, &mut ctx);
        assert_eq!(s.tick_count, 2);
    }

    #[test]
    fn strategy_on_fill() {
        let mut s = CountingStrategy::new();
        let mut ctx = Context::new();
        let fill = Fill {
            instrument: 0,
            order_id: 1,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            remaining: 0,
            timestamp_ns: 0,
        };
        s.on_fill(&fill, &mut ctx);
        assert_eq!(s.fill_count, 1);
    }

    #[test]
    fn strategy_submits_orders_in_on_tick() {
        struct OrderPlacer;
        impl Strategy for OrderPlacer {
            fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
                ctx.submit_limit(instrument, Side::Buy, 100, ctx.bid(instrument));
            }
        }

        let mut s = OrderPlacer;
        let mut ctx = Context::new();
        ctx.market.register(265598);
        ctx.market.quote_mut(0).bid = 150 * PRICE_SCALE;

        s.on_tick(0, &mut ctx);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match orders[0] {
            OrderRequest::SubmitLimit { price, .. } => {
                assert_eq!(price, 150 * PRICE_SCALE);
            }
            _ => panic!("expected SubmitLimit"),
        }
    }
}
