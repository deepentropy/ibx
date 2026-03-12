//! Integration test: hot loop lifecycle (tick → shared state → fill → shared state).

use std::sync::Arc;

use ibx::bridge::SharedState;
use ibx::engine::hot_loop::HotLoop;
use ibx::types::*;

#[test]
fn full_lifecycle() {
    let shared = Arc::new(SharedState::new());
    let mut engine = HotLoop::new(shared.clone(), None, None);
    let aapl = engine.context_mut().register_instrument(265598);

    // Set up market data: spread = $2
    let q = engine.context_mut().quote_mut(aapl);
    q.bid = 150 * PRICE_SCALE;
    q.ask = 152 * PRICE_SCALE;

    // Tick pushes quote to shared state
    engine.inject_tick(aapl);

    let quote = shared.quote(aapl);
    assert_eq!(quote.bid, 150 * PRICE_SCALE);
    assert_eq!(quote.ask, 152 * PRICE_SCALE);

    // Simulate fill → pushed to shared state
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

    let fills = shared.drain_fills();
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].price, 150 * PRICE_SCALE);
}

#[test]
fn multi_instrument_ticks_update_shared_state() {
    let shared = Arc::new(SharedState::new());
    let mut engine = HotLoop::new(shared.clone(), None, None);
    let aapl = engine.context_mut().register_instrument(265598);
    let msft = engine.context_mut().register_instrument(272093);

    engine.context_mut().quote_mut(aapl).bid = 150 * PRICE_SCALE;
    engine.context_mut().quote_mut(msft).bid = 400 * PRICE_SCALE;

    engine.inject_tick(aapl);
    engine.inject_tick(msft);

    let qa = shared.quote(aapl);
    let qm = shared.quote(msft);
    assert_eq!(qa.bid, 150 * PRICE_SCALE);
    assert_eq!(qm.bid, 400 * PRICE_SCALE);
}
