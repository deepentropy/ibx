//! Integration test: full strategy lifecycle (start → tick → fill → position update).

use ib_engine::engine::context::{Context, Strategy};
use ib_engine::engine::hot_loop::HotLoop;
use ib_engine::types::*;

/// Strategy that buys on first tick, places take-profit on fill.
struct MomentumStrategy {
    threshold: Price,
    #[allow(dead_code)]
    started: bool,
    #[allow(dead_code)]
    ticks: u32,
    #[allow(dead_code)]
    fills: u32,
}

impl MomentumStrategy {
    fn new(threshold: Price) -> Self {
        Self {
            threshold,
            started: false,
            ticks: 0,
            fills: 0,
        }
    }
}

impl Strategy for MomentumStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {
        self.started = true;
    }

    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
        self.ticks += 1;
        if ctx.spread(instrument) > self.threshold && ctx.position(instrument) == 0 {
            ctx.submit_limit(instrument, Side::Buy, 100, ctx.bid(instrument));
        }
    }

    fn on_fill(&mut self, fill: &Fill, ctx: &mut Context) {
        self.fills += 1;
        let tp = fill.price + self.threshold * 2;
        ctx.submit_limit(fill.instrument, Side::Sell, fill.qty as u32, tp);
    }
}

#[test]
fn full_lifecycle() {
    let strategy = MomentumStrategy::new(PRICE_SCALE); // $1 threshold

    let mut engine = HotLoop::new(strategy, None);
    let aapl = engine.context_mut().register_instrument(265598);

    // Set up market data: spread = $2 > threshold $1
    let q = engine.context_mut().quote_mut(aapl);
    q.bid = 150 * PRICE_SCALE;
    q.ask = 152 * PRICE_SCALE;

    // Phase 1: tick → strategy submits buy order
    engine.inject_tick(aapl);

    let orders: Vec<_> = engine.context_mut().drain_pending_orders().collect();
    assert_eq!(orders.len(), 1);
    match orders[0] {
        OrderRequest::SubmitLimit {
            side: Side::Buy,
            qty: 100,
            price,
            ..
        } => assert_eq!(price, 150 * PRICE_SCALE),
        _ => panic!("expected buy SubmitLimit"),
    }

    // Phase 2: simulate fill → strategy places take-profit
    let fill = Fill {
        instrument: aapl,
        order_id: 1,
        side: Side::Buy,
        price: 150 * PRICE_SCALE,
        qty: 100,
        remaining: 0,
        commission: 0,
        timestamp_ns: 0,
    };
    engine.inject_fill(&fill);

    assert_eq!(engine.context_mut().position(aapl), 100);

    let orders: Vec<_> = engine.context_mut().drain_pending_orders().collect();
    assert_eq!(orders.len(), 1);
    match orders[0] {
        OrderRequest::SubmitLimit {
            side: Side::Sell,
            qty: 100,
            price,
            ..
        } => {
            // Take-profit = fill price + threshold * 2 = $150 + $2 = $152
            assert_eq!(price, 152 * PRICE_SCALE);
        }
        _ => panic!("expected sell SubmitLimit (take-profit)"),
    }

    // Phase 3: another tick with position held → no new order
    engine.inject_tick(aapl);
    let orders: Vec<_> = engine.context_mut().drain_pending_orders().collect();
    assert!(orders.is_empty());
}

#[test]
fn multi_instrument_isolation() {
    struct Buyer;
    impl Strategy for Buyer {
        fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
            if ctx.position(instrument) == 0 && ctx.bid(instrument) > 0 {
                ctx.submit_limit(instrument, Side::Buy, 50, ctx.bid(instrument));
            }
        }
    }

    let mut engine = HotLoop::new(Buyer, None);
    let aapl = engine.context_mut().register_instrument(265598);
    let msft = engine.context_mut().register_instrument(272093);

    engine.context_mut().quote_mut(aapl).bid = 150 * PRICE_SCALE;
    engine.context_mut().quote_mut(msft).bid = 400 * PRICE_SCALE;

    engine.inject_tick(aapl);
    engine.inject_tick(msft);

    let orders: Vec<_> = engine.context_mut().drain_pending_orders().collect();
    assert_eq!(orders.len(), 2);

    // Each instrument got its own order at its own price
    let prices: Vec<Price> = orders
        .iter()
        .map(|o| match o {
            &OrderRequest::SubmitLimit { price, .. } => price,
            _ => panic!("expected SubmitLimit"),
        })
        .collect();
    assert!(prices.contains(&(150 * PRICE_SCALE)));
    assert!(prices.contains(&(400 * PRICE_SCALE)));
}
