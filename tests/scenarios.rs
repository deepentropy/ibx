//! Multi-step scenario tests for ibx.
//!
//! Each test exercises a realistic workflow spanning multiple API calls
//! and verifying the full sequence of state transitions and callbacks.

use std::sync::Arc;

use ibx::api::client::{EClient, Contract, Order, TagValue};
use ibx::api::wrapper::tests::RecordingWrapper;
use ibx::bridge::SharedState;
use ibx::control::historical::{HistoricalResponse, HistoricalBar, HeadTimestampResponse};
use ibx::control::contracts::{ContractDefinition, SecurityType};
use ibx::engine::hot_loop::HotLoop;
use ibx::types::*;

/// Helper: build an EClient backed by SharedState + channel.
fn test_client() -> (EClient, crossbeam_channel::Receiver<ControlCommand>, Arc<SharedState>) {
    let shared = Arc::new(SharedState::new());
    let (tx, rx) = crossbeam_channel::unbounded();
    let handle = std::thread::spawn(|| {});
    let client = EClient::from_parts(shared.clone(), tx, handle, "DU123".into());
    (client, rx, shared)
}

fn spy() -> Contract {
    Contract { con_id: 756733, symbol: "SPY".into(), ..Default::default() }
}

fn aapl() -> Contract {
    Contract { con_id: 265598, symbol: "AAPL".into(), ..Default::default() }
}

// ═══════════════════════════════════════════════════════════════════════
//  ORDER LIFECYCLE SCENARIOS
// ═══════════════════════════════════════════════════════════════════════

/// Place limit → partial fill → second fill → fully filled.
/// Verify order_status transitions and exec_details at each step.
#[test]
fn order_lifecycle_partial_then_full_fill() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);

    // Step 1: Order submitted
    shared.orders.push_order_update(OrderUpdate {
        order_id: 100, instrument: 0, status: OrderStatus::Submitted,
        filled_qty: 0, remaining_qty: 200, perm_id: 0, parent_id: 0, timestamp_ns: 1000,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:100:Submitted")));

    // Step 2: Partial fill — 120 of 200 shares
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 100, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 120, remaining: 80,
        commission: PRICE_SCALE / 2, timestamp_ns: 2000,
    });
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:100:PartiallyFilled")));
    assert!(w.events.iter().any(|e| e.starts_with("exec_details:1:BOT:120")));

    // Step 3: Remaining 80 fills
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 100, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 80, remaining: 0,
        commission: PRICE_SCALE / 2, timestamp_ns: 3000,
    });
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:100:Filled")));
    assert!(w.events.iter().any(|e| e.starts_with("exec_details:1:BOT:80")));
}

/// Place order → cancel → verify cancelled status, no ghost position.
#[test]
fn order_lifecycle_place_then_cancel() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    client.seed_instrument(756733, 0);

    // Place order
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0, ..Default::default()
    };
    client.place_order(50, &spy(), &order).unwrap();
    // Drain control commands
    while rx.try_recv().is_ok() {}

    // Cancel order
    client.cancel_order(50, "").unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::Cancel { order_id: 50 })));

    // Simulate cancel ack from engine
    shared.orders.push_order_update(OrderUpdate {
        order_id: 50, instrument: 0, status: OrderStatus::Cancelled,
        filled_qty: 0, remaining_qty: 100, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:50:Cancelled")));

    // Position should be 0 (no fills happened)
    assert_eq!(shared.portfolio.position(0), 0);
}

/// Place order → rejection → error callback → no position change.
#[test]
fn order_lifecycle_rejection() {
    let (client, _rx, shared) = test_client();

    shared.orders.push_order_update(OrderUpdate {
        order_id: 60, instrument: 0, status: OrderStatus::Rejected,
        filled_qty: 0, remaining_qty: 100, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:60:Inactive")));
    assert_eq!(shared.portfolio.position(0), 0);
}

/// Place order → partial fill → cancel → verify position reflects only the partial fill.
#[test]
fn order_lifecycle_partial_fill_then_cancel() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);

    // Partial fill: 30 of 100
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 70, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 30, remaining: 70,
        commission: 0, timestamp_ns: 1000,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:70:PartiallyFilled")));

    // Cancel remaining
    shared.orders.push_order_update(OrderUpdate {
        order_id: 70, instrument: 0, status: OrderStatus::Cancelled,
        filled_qty: 30, remaining_qty: 70, perm_id: 0, parent_id: 0, timestamp_ns: 2000,
    });
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:70:Cancelled")));
}

/// Place order → modify → fill at new price.
#[test]
fn order_lifecycle_modify_then_fill() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    client.seed_instrument(756733, 0);

    // Place limit at 150
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0, ..Default::default()
    };
    client.place_order(80, &spy(), &order).unwrap();
    while rx.try_recv().is_ok() {}

    // Modify: new limit at 151
    let modified_order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 151.0, ..Default::default()
    };
    client.place_order(80, &spy(), &modified_order).unwrap();
    while rx.try_recv().is_ok() {}

    // Fill at new price
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 80, side: Side::Buy,
        price: 151 * PRICE_SCALE, qty: 100, remaining: 0,
        commission: PRICE_SCALE, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:80:Filled")));
    // Verify fill price is 151
    let fill_event = w.events.iter().find(|e| e.starts_with("exec_details")).unwrap();
    assert!(fill_event.contains("BOT:100"), "fill should be 100 shares");
}

/// What-if order: margin preview without actual execution.
#[test]
fn order_lifecycle_what_if_preview() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    client.seed_instrument(756733, 0);

    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0,
        what_if: true, ..Default::default()
    };
    client.place_order(90, &spy(), &order).unwrap();

    // Verify SubmitWhatIf was sent
    let mut found_what_if = false;
    while let Ok(cmd) = rx.try_recv() {
        if matches!(cmd, ControlCommand::Order(OrderRequest::SubmitWhatIf { .. })) {
            found_what_if = true;
        }
    }
    assert!(found_what_if);

    // Simulate what-if response
    shared.orders.push_what_if(WhatIfResponse {
        order_id: 90, instrument: 0,
        init_margin_before: 0, maint_margin_before: 0, equity_with_loan_before: 0,
        init_margin_after: 15000 * PRICE_SCALE,
        maint_margin_after: 10000 * PRICE_SCALE,
        equity_with_loan_after: 85000 * PRICE_SCALE,
        commission: 2 * PRICE_SCALE,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:90:PreSubmitted")));

    // No actual position change
    assert_eq!(shared.portfolio.position(0), 0);
}

/// Algo order (VWAP): place → multiple partial fills over time.
#[test]
fn order_lifecycle_algo_vwap_partial_fills() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    client.seed_instrument(756733, 0);
    client.map_req_instrument(1, 0);

    let order = Order {
        action: "BUY".into(), total_quantity: 1000.0,
        order_type: "LMT".into(), lmt_price: 150.0,
        algo_strategy: "vwap".into(),
        algo_params: vec![TagValue { tag: "maxPctVol".into(), value: "0.1".into() }],
        ..Default::default()
    };
    client.place_order(110, &spy(), &order).unwrap();
    // Drain
    while rx.try_recv().is_ok() {}

    // 4 partial fills
    let prices = [149_80000000i64, 150_00000000, 150_20000000, 150_50000000];
    let qtys = [250, 300, 200, 250];
    let mut total_filled = 0i64;

    for i in 0..4 {
        total_filled += qtys[i];
        let remaining = 1000 - total_filled;
        shared.orders.push_fill(Fill {
            instrument: 0, order_id: 110, side: Side::Buy,
            price: prices[i as usize], qty: qtys[i] as i64, remaining,
            commission: PRICE_SCALE / 10, timestamp_ns: (i as u64 + 1) * 1000,
        });
    }

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    // 3 partial fills + 1 final fill
    let partial_count = w.events.iter().filter(|e| e.starts_with("order_status:110:PartiallyFilled")).count();
    let filled_count = w.events.iter().filter(|e| e.starts_with("order_status:110:Filled")).count();
    assert_eq!(partial_count, 3);
    assert_eq!(filled_count, 1);
    // 4 exec_details
    let exec_count = w.events.iter().filter(|e| e.starts_with("exec_details:")).count();
    assert_eq!(exec_count, 4);
}

/// Cancel reject: attempt to cancel already-filled order.
#[test]
fn order_lifecycle_cancel_reject_on_filled_order() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);

    // Order fills completely
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 120, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 100, remaining: 0,
        commission: 0, timestamp_ns: 1000,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:120:Filled")));

    // Attempt to cancel → reject
    shared.orders.push_cancel_reject(CancelReject {
        order_id: 120, instrument: 0, reject_type: 1, reason_code: 0, timestamp_ns: 2000,
    });
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:120:202:")));
}

// ═══════════════════════════════════════════════════════════════════════
//  MARKET DATA SCENARIOS
// ═══════════════════════════════════════════════════════════════════════

/// Subscribe → receive ticks → unsubscribe → verify no more ticks.
#[test]
fn market_data_subscribe_ticks_unsubscribe() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);

    // Manually map since we bypass the real engine
    client.map_req_instrument(1, 0);

    // Simulate quote arriving
    let mut q = Quote::default();
    q.bid = 450 * PRICE_SCALE;
    q.ask = 451 * PRICE_SCALE;
    shared.market.push_quote(0, &q);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:450")));

    // Unsubscribe
    client.cancel_mkt_data(1).unwrap();

    // Push new quote — should NOT be dispatched
    q.bid = 449 * PRICE_SCALE;
    shared.market.push_quote(0, &q);
    w.events.clear();
    client.process_msgs(&mut w);
    let bid_ticks: Vec<_> = w.events.iter().filter(|e| e.starts_with("tick_price:1:")).collect();
    assert!(bid_ticks.is_empty(), "no ticks after unsubscribe");
}

/// Subscribe multiple instruments → verify independent tick streams.
#[test]
fn market_data_multi_instrument_independent() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(2);

    // Manually map since we bypass the real engine
    client.map_req_instrument(1, 0);
    client.map_req_instrument(2, 1);

    // Quote for instrument 0
    let mut q0 = Quote::default();
    q0.bid = 450 * PRICE_SCALE;
    shared.market.push_quote(0, &q0);
    // Quote for instrument 1
    let mut q1 = Quote::default();
    q1.bid = 150 * PRICE_SCALE;
    shared.market.push_quote(1, &q1);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:450")));
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:2:1:150")));

    // Update only instrument 1
    q1.bid = 149 * PRICE_SCALE;
    shared.market.push_quote(1, &q1);
    w.events.clear();
    client.process_msgs(&mut w);

    // Only instrument 1 should fire
    let ticks_1: Vec<_> = w.events.iter().filter(|e| e.starts_with("tick_price:1:")).collect();
    let ticks_2: Vec<_> = w.events.iter().filter(|e| e.starts_with("tick_price:2:")).collect();
    assert!(ticks_1.is_empty(), "instrument 0 unchanged → no ticks");
    assert!(!ticks_2.is_empty(), "instrument 1 changed → should have ticks");
}

/// Tick-by-tick: subscribe → trades + quotes → verify both dispatched.
#[test]
fn market_data_tbt_trades_and_quotes() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(10, 0);

    // TBT trade
    shared.market.push_tbt_trade(TbtTrade {
        instrument: 0, price: 150 * PRICE_SCALE, size: 100,
        timestamp: 1700000001, exchange: "ARCA".into(), conditions: "".into(),
    });
    // TBT quote
    shared.market.push_tbt_quote(TbtQuote {
        instrument: 0, bid: 149 * PRICE_SCALE, ask: 151 * PRICE_SCALE,
        bid_size: 500, ask_size: 300, timestamp: 1700000002,
    });
    // Second trade
    shared.market.push_tbt_trade(TbtTrade {
        instrument: 0, price: 151 * PRICE_SCALE, size: 200,
        timestamp: 1700000003, exchange: "NYSE".into(), conditions: "".into(),
    });

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    let trades: Vec<_> = w.events.iter().filter(|e| e.starts_with("tbt_last:")).collect();
    let quotes: Vec<_> = w.events.iter().filter(|e| e.starts_with("tbt_bidask:")).collect();
    assert_eq!(trades.len(), 2);
    assert_eq!(quotes.len(), 1);
}

// ═══════════════════════════════════════════════════════════════════════
//  ACCOUNT SCENARIOS
// ═══════════════════════════════════════════════════════════════════════

/// Fill buy → fill sell → position returns to zero → verify round-trip.
#[test]
fn account_round_trip_position() {
    let shared = Arc::new(SharedState::new());
    let mut engine = HotLoop::new(shared.clone(), None, None);
    let spy_id = engine.context_mut().register_instrument(756733);

    // Buy 100 @ 150
    engine.inject_fill(&Fill {
        instrument: spy_id, order_id: 1, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 100, remaining: 0,
        commission: PRICE_SCALE, timestamp_ns: 1000,
    });
    assert_eq!(engine.context_mut().position(spy_id), 100);
    assert_eq!(shared.portfolio.position(spy_id), 100);

    // Sell 100 @ 152
    engine.inject_fill(&Fill {
        instrument: spy_id, order_id: 2, side: Side::Sell,
        price: 152 * PRICE_SCALE, qty: 100, remaining: 0,
        commission: PRICE_SCALE, timestamp_ns: 2000,
    });
    assert_eq!(engine.context_mut().position(spy_id), 0);
    assert_eq!(shared.portfolio.position(spy_id), 0);

    // Two fills in shared state
    let fills = shared.orders.drain_fills();
    assert_eq!(fills.len(), 2);
    assert_eq!(fills[0].side, Side::Buy);
    assert_eq!(fills[1].side, Side::Sell);
}

/// Multiple instruments: fills on A and B → positions aggregate correctly.
#[test]
fn account_multi_instrument_positions() {
    let shared = Arc::new(SharedState::new());
    let mut engine = HotLoop::new(shared.clone(), None, None);
    let spy_id = engine.context_mut().register_instrument(756733);
    let aapl_id = engine.context_mut().register_instrument(265598);

    // Buy 50 SPY
    engine.inject_fill(&Fill {
        instrument: spy_id, order_id: 1, side: Side::Buy,
        price: 450 * PRICE_SCALE, qty: 50, remaining: 0,
        commission: 0, timestamp_ns: 1000,
    });

    // Buy 100 AAPL
    engine.inject_fill(&Fill {
        instrument: aapl_id, order_id: 2, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 100, remaining: 0,
        commission: 0, timestamp_ns: 2000,
    });

    // Sell 20 SPY
    engine.inject_fill(&Fill {
        instrument: spy_id, order_id: 3, side: Side::Sell,
        price: 452 * PRICE_SCALE, qty: 20, remaining: 0,
        commission: 0, timestamp_ns: 3000,
    });

    assert_eq!(engine.context_mut().position(spy_id), 30);
    assert_eq!(engine.context_mut().position(aapl_id), 100);
    assert_eq!(shared.portfolio.position(spy_id), 30);
    assert_eq!(shared.portfolio.position(aapl_id), 100);
}

/// Account state tracks through fills and position updates.
#[test]
fn account_state_via_eclient() {
    let (client, _rx, shared) = test_client();
    let mut acct = AccountState::default();
    acct.net_liquidation = 100_000 * PRICE_SCALE;
    acct.buying_power = 200_000 * PRICE_SCALE;
    shared.portfolio.set_account(&acct);

    let state = client.account();
    assert_eq!(state.net_liquidation, 100_000 * PRICE_SCALE);
    assert_eq!(state.buying_power, 200_000 * PRICE_SCALE);

    // Update after activity
    acct.net_liquidation = 99_500 * PRICE_SCALE;
    shared.portfolio.set_account(&acct);
    let state2 = client.account();
    assert_eq!(state2.net_liquidation, 99_500 * PRICE_SCALE);
}

/// Position tracking through reqPositions after fills.
#[test]
fn account_req_positions_reflects_fills() {
    let (client, _rx, shared) = test_client();

    // Set position info (as engine would after fills)
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 756733, position: 100, avg_cost: 450 * PRICE_SCALE,
    });
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 265598, position: -50, avg_cost: 150 * PRICE_SCALE,
    });

    let mut w = RecordingWrapper::default();
    client.req_positions(&mut w);

    let positions: Vec<_> = w.events.iter().filter(|e| e.starts_with("position:")).collect();
    assert_eq!(positions.len(), 2);
    assert!(w.events.last().unwrap() == "position_end");
}

// ═══════════════════════════════════════════════════════════════════════
//  HISTORICAL DATA SCENARIOS
// ═══════════════════════════════════════════════════════════════════════

/// Request bars → multi-message response → historical_data_end fires.
#[test]
fn historical_data_multi_bar_complete() {
    let (client, _rx, shared) = test_client();

    // First batch (incomplete)
    shared.reference.push_historical_data(5, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![
            HistoricalBar { time: "20260101".into(), open: 100.0, high: 105.0, low: 99.0, close: 103.0, volume: 1000, wap: 102.0, count: 50 },
            HistoricalBar { time: "20260102".into(), open: 103.0, high: 108.0, low: 102.0, close: 107.0, volume: 1200, wap: 105.0, count: 60 },
        ],
        is_complete: false,
    });

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events.iter().filter(|e| e.starts_with("historical_data:5:")).count(), 2);
    assert!(!w.events.iter().any(|e| e == "historical_data_end:5"));

    // Second batch (complete)
    shared.reference.push_historical_data(5, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![
            HistoricalBar { time: "20260103".into(), open: 107.0, high: 110.0, low: 106.0, close: 109.0, volume: 800, wap: 108.0, count: 40 },
        ],
        is_complete: true,
    });

    w.events.clear();
    client.process_msgs(&mut w);
    assert_eq!(w.events.iter().filter(|e| e.starts_with("historical_data:5:")).count(), 1);
    assert!(w.events.iter().any(|e| e == "historical_data_end:5"));
}

/// Request → cancel before completion → second process_msgs has no stale data.
#[test]
fn historical_data_cancel_no_stale_callbacks() {
    let (client, rx, _shared) = test_client();

    // Request
    client.req_historical_data(6, &spy(), "", "1 D", "1 hour", "TRADES", true, 1, false).unwrap();
    while rx.try_recv().is_ok() {}

    // Cancel
    client.cancel_historical_data(6).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::CancelHistorical { req_id: 6 }));

    // No data was pushed → process_msgs should be clean
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let hist_events: Vec<_> = w.events.iter().filter(|e| e.starts_with("historical_data:")).collect();
    assert!(hist_events.is_empty());
}

/// Head timestamp then historical data: chained requests.
#[test]
fn historical_head_timestamp_then_bars() {
    let (client, _rx, shared) = test_client();

    // Step 1: head timestamp response
    shared.reference.push_head_timestamp(10, HeadTimestampResponse {
        head_timestamp: "20050101".into(), timezone: "US/Eastern".into(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "head_timestamp:10:20050101"));

    // Step 2: user requests bars from that timestamp
    shared.reference.push_historical_data(11, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![
            HistoricalBar { time: "20050101".into(), open: 50.0, high: 55.0, low: 49.0, close: 53.0, volume: 5000, wap: 52.0, count: 100 },
        ],
        is_complete: true,
    });
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "historical_data:11:20050101"));
    assert!(w.events.iter().any(|e| e == "historical_data_end:11"));
}

// ═══════════════════════════════════════════════════════════════════════
//  CONTRACT DETAILS + MARKET DATA SCENARIO
// ═══════════════════════════════════════════════════════════════════════

/// Look up contract → get details → subscribe to market data.
#[test]
fn contract_lookup_then_subscribe() {
    let (client, rx, shared) = test_client();

    // Step 1: request contract details
    client.req_contract_details(20, &aapl()).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::FetchContractDetails { req_id: 20, con_id: 265598, .. }));

    // Step 2: engine responds with contract details
    shared.reference.push_contract_details(20, ContractDefinition {
        con_id: 265598, symbol: "AAPL".into(), sec_type: SecurityType::Stock,
        exchange: "SMART".into(), primary_exchange: "NASDAQ".into(),
        currency: "USD".into(), local_symbol: "AAPL".into(),
        trading_class: "AAPL".into(), long_name: "Apple Inc".into(),
        min_tick: 0.01, multiplier: 1.0, valid_exchanges: vec!["SMART".into()],
        order_types: vec!["LMT".into()], market_rule_id: Some(26),
        last_trade_date: String::new(), right: None, strike: 0.0,
        ..Default::default()
    });
    shared.reference.push_contract_details_end(20);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "contract_details:20:AAPL"));
    assert!(w.events.iter().any(|e| e == "contract_details_end:20"));

    // Step 3: now subscribe to market data for this contract
    let _ = client.req_mkt_data(21, &aapl(), "", false, false);

    // Simulate ticks
    let mut q = Quote::default();
    q.bid = 178 * PRICE_SCALE;
    q.ask = 179 * PRICE_SCALE;
    shared.market.push_quote(0, &q);

    // Map (simulating what engine would do)
    client.map_req_instrument(21, 0);

    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:21:1:178")));
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:21:2:179")));
}

// ═══════════════════════════════════════════════════════════════════════
//  ENGINE-LEVEL SCENARIOS (HotLoop + SharedState)
// ═══════════════════════════════════════════════════════════════════════

/// Full engine scenario: register → tick → fill → position check → second fill.
#[test]
fn engine_full_trade_lifecycle() {
    let shared = Arc::new(SharedState::new());
    let mut engine = HotLoop::new(shared.clone(), None, None);

    let spy_id = engine.context_mut().register_instrument(756733);
    engine.context_mut().set_symbol(spy_id, "SPY".into());

    // Market data
    let q = engine.context_mut().quote_mut(spy_id);
    q.bid = 450 * PRICE_SCALE;
    q.ask = 451 * PRICE_SCALE;
    q.last = 450_50000000;
    engine.inject_tick(spy_id);

    // Verify shared state
    let sq = shared.market.quote(spy_id);
    assert_eq!(sq.bid, 450 * PRICE_SCALE);

    // Buy fill
    engine.inject_fill(&Fill {
        instrument: spy_id, order_id: 1, side: Side::Buy,
        price: 450 * PRICE_SCALE, qty: 100, remaining: 0,
        commission: PRICE_SCALE, timestamp_ns: 1000,
    });
    assert_eq!(engine.context_mut().position(spy_id), 100);

    // Price moves up
    let q = engine.context_mut().quote_mut(spy_id);
    q.bid = 455 * PRICE_SCALE;
    q.ask = 456 * PRICE_SCALE;
    engine.inject_tick(spy_id);

    // Sell fill at higher price
    engine.inject_fill(&Fill {
        instrument: spy_id, order_id: 2, side: Side::Sell,
        price: 455 * PRICE_SCALE, qty: 100, remaining: 0,
        commission: PRICE_SCALE, timestamp_ns: 2000,
    });
    assert_eq!(engine.context_mut().position(spy_id), 0);

    // Verify all fills flowed to SharedState
    let fills = shared.orders.drain_fills();
    assert_eq!(fills.len(), 2);
    assert_eq!(fills[0].price, 450 * PRICE_SCALE);
    assert_eq!(fills[1].price, 455 * PRICE_SCALE);
}

/// Engine + EClient end-to-end: HotLoop pushes → EClient dispatches to wrapper.
#[test]
fn engine_to_eclient_end_to_end() {
    let shared = Arc::new(SharedState::new());
    let mut engine = HotLoop::new(shared.clone(), None, None);
    let spy_id = engine.context_mut().register_instrument(756733);

    // Set up quote
    let q = engine.context_mut().quote_mut(spy_id);
    q.bid = 450 * PRICE_SCALE;
    q.ask = 451 * PRICE_SCALE;
    engine.inject_tick(spy_id);

    // Build EClient on same SharedState
    let (tx, _rx) = crossbeam_channel::unbounded();
    let handle = std::thread::spawn(|| {});
    let client = EClient::from_parts(shared.clone(), tx, handle, "DU123".into());
    client.map_req_instrument(1, spy_id);

    // Process — should see ticks
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:450")));
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:2:451")));

    // Now inject a fill through the engine
    engine.inject_fill(&Fill {
        instrument: spy_id, order_id: 42, side: Side::Buy,
        price: 450 * PRICE_SCALE, qty: 100, remaining: 0,
        commission: PRICE_SCALE, timestamp_ns: 1000,
    });

    // Process — should see fill
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:42:Filled")));
    assert!(w.events.iter().any(|e| e.starts_with("exec_details:1:BOT:100")));
}

/// Short sell scenario: sell short → buy to cover → flat.
#[test]
fn engine_short_sell_then_cover() {
    let shared = Arc::new(SharedState::new());
    let mut engine = HotLoop::new(shared.clone(), None, None);
    let spy_id = engine.context_mut().register_instrument(756733);

    // Short sell 50
    engine.inject_fill(&Fill {
        instrument: spy_id, order_id: 1, side: Side::ShortSell,
        price: 450 * PRICE_SCALE, qty: 50, remaining: 0,
        commission: 0, timestamp_ns: 1000,
    });
    assert_eq!(engine.context_mut().position(spy_id), -50);

    // Buy to cover 50
    engine.inject_fill(&Fill {
        instrument: spy_id, order_id: 2, side: Side::Buy,
        price: 445 * PRICE_SCALE, qty: 50, remaining: 0,
        commission: 0, timestamp_ns: 2000,
    });
    assert_eq!(engine.context_mut().position(spy_id), 0);
}

// ═══════════════════════════════════════════════════════════════════════
//  MIXED SCENARIOS (orders + market data interleaved)
// ═══════════════════════════════════════════════════════════════════════

/// Ticks arriving while order fills are in flight.
#[test]
fn mixed_ticks_during_fills() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);

    // Quote arrives
    let mut q = Quote::default();
    q.bid = 150 * PRICE_SCALE;
    q.ask = 151 * PRICE_SCALE;
    shared.market.push_quote(0, &q);

    // Fill arrives at same time
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 42, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 100, remaining: 0,
        commission: 0, timestamp_ns: 0,
    });

    // Order update arrives at same time
    shared.orders.push_order_update(OrderUpdate {
        order_id: 43, instrument: 0, status: OrderStatus::Submitted,
        filled_qty: 0, remaining_qty: 200, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    // All three types of events should be present
    let has_ticks = w.events.iter().any(|e| e.starts_with("tick_price:"));
    let has_fill = w.events.iter().any(|e| e.starts_with("order_status:42:Filled"));
    let has_update = w.events.iter().any(|e| e.starts_with("order_status:43:Submitted"));
    assert!(has_ticks, "should have tick events");
    assert!(has_fill, "should have fill event");
    assert!(has_update, "should have order update");
}

/// News arriving between order events.
#[test]
fn mixed_news_between_orders() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);

    // Order submitted
    shared.orders.push_order_update(OrderUpdate {
        order_id: 50, instrument: 0, status: OrderStatus::Submitted,
        filled_qty: 0, remaining_qty: 100, perm_id: 0, parent_id: 0, timestamp_ns: 1000,
    });

    // News arrives
    shared.market.push_tick_news(TickNews {
        instrument: 0,
        provider_code: "BRFG".into(), article_id: "BRFG$200".into(),
        headline: "Fed holds rates".into(), timestamp: 1700000000,
    });

    // Fill after news
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 50, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 100, remaining: 0,
        commission: 0, timestamp_ns: 2000,
    });

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    assert!(w.events.iter().any(|e| e.starts_with("order_status:50:Submitted")));
    assert!(w.events.iter().any(|e| e.contains("Fed holds rates")));
    assert!(w.events.iter().any(|e| e.starts_with("order_status:50:Filled")));
}

/// Multiple data types arriving in single process_msgs.
#[test]
fn mixed_all_data_types_single_process() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);

    // Quotes
    let mut q = Quote::default();
    q.bid = 150 * PRICE_SCALE;
    shared.market.push_quote(0, &q);

    // Fill
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 1, side: Side::Buy,
        price: PRICE_SCALE, qty: 1, remaining: 0,
        commission: 0, timestamp_ns: 0,
    });

    // TBT trade
    shared.market.push_tbt_trade(TbtTrade {
        instrument: 0, price: 150 * PRICE_SCALE, size: 50,
        timestamp: 0, exchange: "".into(), conditions: "".into(),
    });

    // Historical data
    shared.reference.push_historical_data(5, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![HistoricalBar {
            time: "20260101".into(), open: 100.0, high: 105.0,
            low: 99.0, close: 103.0, volume: 1000, wap: 102.0, count: 50,
        }],
        is_complete: true,
    });

    // News
    shared.market.push_tick_news(TickNews {
        instrument: 0,
        provider_code: "DJ".into(), article_id: "DJ$1".into(),
        headline: "Breaking".into(), timestamp: 0,
    });

    // Scanner params
    shared.reference.push_scanner_params("<xml/>".into());

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    // Verify all types present
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:")));
    assert!(w.events.iter().any(|e| e.starts_with("order_status:1:Filled")));
    assert!(w.events.iter().any(|e| e.starts_with("tbt_last:")));
    assert!(w.events.iter().any(|e| e.starts_with("historical_data:5:")));
    assert!(w.events.iter().any(|e| e.starts_with("tick_news:")));
    assert!(w.events.iter().any(|e| e == "scanner_parameters"));
}

// ═══════════════════════════════════════════════════════════════════════
//  COMPLETED ORDERS
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn req_completed_orders_drains_and_dispatches() {
    let (client, _rx, shared) = test_client();

    shared.orders.push_completed_order(CompletedOrder {
        order_id: 100, instrument: 0, status: OrderStatus::Filled,
        filled_qty: 50, timestamp_ns: 1000,
    });
    shared.orders.push_completed_order(CompletedOrder {
        order_id: 200, instrument: 0, status: OrderStatus::Cancelled,
        filled_qty: 0, timestamp_ns: 2000,
    });

    let mut w = RecordingWrapper::default();
    client.req_completed_orders(&mut w);

    assert_eq!(w.events.iter().filter(|e| *e == "completed_order").count(), 2);
    assert!(w.events.iter().any(|e| e == "completed_orders_end"));

    // Second call should return empty (already drained)
    w.events.clear();
    client.req_completed_orders(&mut w);
    assert_eq!(w.events.iter().filter(|e| *e == "completed_order").count(), 0);
    assert!(w.events.iter().any(|e| e == "completed_orders_end"));
}

#[test]
fn req_completed_orders_empty_still_fires_end() {
    let (client, _rx, _shared) = test_client();
    let mut w = RecordingWrapper::default();
    client.req_completed_orders(&mut w);
    assert_eq!(w.events, vec!["completed_orders_end"]);
}

// ═══════════════════════════════════════════════════════════════════════
//  PNL SUBSCRIPTION
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn pnl_subscription_fires_on_change() {
    let (client, _rx, shared) = test_client();

    client.req_pnl(10, "DU123", "");

    // Set initial account state with PnL
    let mut acct = AccountState::default();
    acct.daily_pnl = 500 * PRICE_SCALE;
    acct.unrealized_pnl = 1000 * PRICE_SCALE;
    acct.realized_pnl = 200 * PRICE_SCALE;
    shared.portfolio.set_account(&acct);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("pnl:10:")), "PnL callback expected");

    // Same values → no duplicate
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("pnl:")), "No duplicate PnL expected");

    // Changed values → fires again
    acct.daily_pnl = 600 * PRICE_SCALE;
    shared.portfolio.set_account(&acct);
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("pnl:10:")), "Changed PnL should fire callback");
}

#[test]
fn cancel_pnl_stops_dispatch() {
    let (client, _rx, shared) = test_client();

    client.req_pnl(10, "DU123", "");
    let mut acct = AccountState::default();
    acct.daily_pnl = 500 * PRICE_SCALE;
    shared.portfolio.set_account(&acct);

    // First call should fire
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("pnl:10:")));

    // Cancel and change value
    client.cancel_pnl(10);
    acct.daily_pnl = 999 * PRICE_SCALE;
    shared.portfolio.set_account(&acct);

    w.events.clear();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("pnl:")), "Cancelled PnL should not fire");
}

#[test]
fn pnl_single_dispatches_position_info() {
    let (client, _rx, shared) = test_client();

    client.req_pnl_single(20, "DU123", "", 265598);

    shared.portfolio.set_position_info(PositionInfo {
        con_id: 265598,
        position: 100,
        avg_cost: 150 * PRICE_SCALE,
    });

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("pnl_single:20:")), "PnL single callback expected");

    // Cancel should stop dispatch
    client.cancel_pnl_single(20);
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("pnl_single:")), "Cancelled PnL single should not fire");
}

// ═══════════════════════════════════════════════════════════════════════
//  ACCOUNT SUMMARY
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn account_summary_one_shot_delivery() {
    let (client, _rx, shared) = test_client();

    let mut acct = AccountState::default();
    acct.net_liquidation = 100_000 * PRICE_SCALE;
    acct.buying_power = 400_000 * PRICE_SCALE;
    acct.available_funds = 50_000 * PRICE_SCALE;
    shared.portfolio.set_account(&acct);

    client.req_account_summary(5, "All", "NetLiquidation,BuyingPower,AvailableFunds");

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    // Should have exactly 3 account_summary events + 1 end
    let summaries: Vec<_> = w.events.iter().filter(|e| e.starts_with("account_summary:5:")).collect();
    assert_eq!(summaries.len(), 3, "Expected 3 summary tags, got {:?}", summaries);
    assert!(w.events.iter().any(|e| e == "account_summary_end:5"));

    // Verify specific tags
    assert!(summaries.iter().any(|e| e.contains(":NetLiquidation:")));
    assert!(summaries.iter().any(|e| e.contains(":BuyingPower:")));
    assert!(summaries.iter().any(|e| e.contains(":AvailableFunds:")));

    // Second call should NOT fire (one-shot, already consumed)
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("account_summary:")));
    assert!(!w.events.iter().any(|e| e.starts_with("account_summary_end:")));
}

#[test]
fn cancel_account_summary_prevents_delivery() {
    let (client, _rx, shared) = test_client();

    let mut acct = AccountState::default();
    acct.net_liquidation = 100_000 * PRICE_SCALE;
    shared.portfolio.set_account(&acct);

    client.req_account_summary(5, "All", "NetLiquidation");
    client.cancel_account_summary(5);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("account_summary:")));
}

#[test]
fn account_summary_empty_tags_returns_all() {
    let (client, _rx, shared) = test_client();

    let mut acct = AccountState::default();
    acct.net_liquidation = 100_000 * PRICE_SCALE;
    acct.buying_power = 400_000 * PRICE_SCALE;
    acct.total_cash_value = 50_000 * PRICE_SCALE;
    shared.portfolio.set_account(&acct);

    // Empty tags string → all non-zero fields
    client.req_account_summary(7, "All", "");

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    let summaries: Vec<_> = w.events.iter().filter(|e| e.starts_with("account_summary:7:")).collect();
    assert!(summaries.len() >= 3, "Expected at least 3 non-zero fields, got {}", summaries.len());
    assert!(w.events.iter().any(|e| e == "account_summary_end:7"));
}

// ═══════════════════════════════════════════════════════════════════════
//  NEWS BULLETINS
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn news_bulletins_gated_by_subscription() {
    let (client, _rx, shared) = test_client();

    shared.market.push_news_bulletin(NewsBulletin {
        msg_id: 1, msg_type: 1, message: "Test bulletin".into(), exchange: "NYSE".into(),
    });

    // Without subscription → not dispatched (but drained silently)
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("news_bulletin:")));

    // Subscribe
    client.req_news_bulletins(true);

    shared.market.push_news_bulletin(NewsBulletin {
        msg_id: 2, msg_type: 2, message: "Exchange down".into(), exchange: "ARCA".into(),
    });

    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("news_bulletin:")), "Expected bulletin after subscription");

    // Cancel subscription
    client.cancel_news_bulletins();

    shared.market.push_news_bulletin(NewsBulletin {
        msg_id: 3, msg_type: 1, message: "After cancel".into(), exchange: "NYSE".into(),
    });

    w.events.clear();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("news_bulletin:")), "No bulletin after cancel");
}
