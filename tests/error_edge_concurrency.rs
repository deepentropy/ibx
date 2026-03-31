//! Error path, edge case, and concurrency tests for ibx.
//!
//! Validates that the library handles bad inputs, boundary conditions,
//! and concurrent access without panics or data races.

use std::sync::Arc;
use std::thread;

use ibx::api::client::{EClient, Contract, Order};
use ibx::api::wrapper::tests::RecordingWrapper;
use ibx::bridge::SharedState;
use ibx::control::historical::{HistoricalResponse, HistoricalBar};
use ibx::engine::hot_loop::HotLoop;
use ibx::protocol::fix;
use ibx::types::*;

fn test_client() -> (EClient, crossbeam_channel::Receiver<ControlCommand>, Arc<SharedState>) {
    let shared = Arc::new(SharedState::new());
    let (tx, rx) = crossbeam_channel::unbounded();
    let handle = thread::spawn(|| {});
    let client = EClient::from_parts(shared.clone(), tx, handle, "DU123".into());
    // Pre-seed instrument mappings so tests don't need a running hot loop.
    client.seed_instrument(756733, 0);
    client.seed_instrument(0, 1);
    (client, rx, shared)
}

fn spy() -> Contract {
    Contract { con_id: 756733, symbol: "SPY".into(), ..Default::default() }
}

// ═══════════════════════════════════════════════════════════════════════
//  ERROR PATHS — place_order
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn place_order_invalid_action_returns_error() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "INVALID".into(), total_quantity: 100.0,
        order_type: "MKT".into(), ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Invalid action"));
}

#[test]
fn place_order_empty_action_returns_error() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: String::new(), total_quantity: 100.0,
        order_type: "MKT".into(), ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
}

#[test]
fn place_order_unsupported_order_type_returns_error() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "NONSENSE".into(), ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Unsupported order type"));
}

#[test]
fn place_order_unsupported_algo_returns_error() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0,
        algo_strategy: "unknown_algo".into(),
        ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Unsupported algo"));
}

#[test]
fn place_order_zero_con_id_still_sends() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let contract = Contract { con_id: 0, symbol: "TEST".into(), ..Default::default() };
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "MKT".into(), ..Default::default()
    };
    // Should not error — the engine handles zero con_id
    let result = client.place_order(1, &contract, &order);
    assert!(result.is_ok());
    // Drain to avoid channel filling
    while rx.try_recv().is_ok() {}
}

// ═══════════════════════════════════════════════════════════════════════
//  ERROR PATHS — cancel operations on non-existent targets
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn cancel_mkt_data_unknown_req_id_no_panic() {
    let (client, rx, _shared) = test_client();
    client.cancel_mkt_data(999).unwrap();
    assert!(rx.try_recv().is_err()); // no command sent
}

#[test]
fn cancel_tick_by_tick_unknown_req_id_no_panic() {
    let (client, rx, _shared) = test_client();
    client.cancel_tick_by_tick_data(999).unwrap();
    assert!(rx.try_recv().is_err());
}

#[test]
fn cancel_order_nonexistent_sends_cancel_anyway() {
    // cancel_order doesn't validate — it sends the command and lets engine deal with it
    let (client, rx, _shared) = test_client();
    client.cancel_order(999999, "").unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::Cancel { order_id: 999999 })));
}

#[test]
fn req_global_cancel_no_instruments_no_commands() {
    let (client, rx, _shared) = test_client();
    client.req_global_cancel().unwrap();
    assert!(rx.try_recv().is_err());
}

// ═══════════════════════════════════════════════════════════════════════
//  ERROR PATHS — disconnect during activity
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn disconnect_during_active_subscription() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);

    // Subscribe
    let _ = client.req_mkt_data(1, &spy(), "", false, false);
    while rx.try_recv().is_ok() {}

    // Disconnect
    client.disconnect();
    assert!(!client.is_connected());

    // Push quote after disconnect — process_msgs should still work (no panic)
    let mut q = Quote::default();
    q.bid = 150 * PRICE_SCALE;
    shared.market.push_quote(0, &q);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    // Might or might not dispatch depending on mapping — key is no panic
}

#[test]
fn disconnect_during_pending_order_uncertain_status() {
    let (client, _rx, shared) = test_client();

    // Order was pending when we disconnect
    shared.orders.push_order_update(OrderUpdate {
        order_id: 50, instrument: 0, status: OrderStatus::Uncertain,
        filled_qty: 0, remaining_qty: 100, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:50:Unknown")));
}

// ═══════════════════════════════════════════════════════════════════════
//  ERROR PATHS — fill dedup (engine level)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn fill_dedup_duplicate_exec_id_no_double_position() {
    let shared = Arc::new(SharedState::new());
    let mut engine = HotLoop::new(shared.clone(), None, None);
    engine.context_mut().register_instrument(265598);

    engine.context_mut().insert_order(ibx::types::Order {
        order_id: 70, instrument: 0, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 100, filled: 0,
        status: OrderStatus::Submitted,
        ord_type: b'2', tif: b'0', stop_price: 0,
    });

    // Same ExecID sent twice
    let msg = fix::fix_build(&[
        (35, "8"), (11, "70"), (17, "EXEC_DUP1"),
        (39, "2"), (150, "F"),
        (31, "150.0"), (32, "100"), (151, "0"),
    ], 1);
    engine.inject_ccp_message(&msg);
    engine.inject_ccp_message(&msg);

    // Only one fill
    assert_eq!(shared.orders.drain_fills().len(), 1);
    assert_eq!(engine.context_mut().position(0), 100);
}

#[test]
fn fill_dedup_different_exec_ids_both_count() {
    let shared = Arc::new(SharedState::new());
    let mut engine = HotLoop::new(shared.clone(), None, None);
    engine.context_mut().register_instrument(265598);

    engine.context_mut().insert_order(ibx::types::Order {
        order_id: 71, instrument: 0, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 200, filled: 0,
        status: OrderStatus::Submitted,
        ord_type: b'2', tif: b'0', stop_price: 0,
    });

    let msg_a = fix::fix_build(&[
        (35, "8"), (11, "71"), (17, "EXEC_A1"),
        (39, "1"), (150, "1"),
        (31, "150.0"), (32, "100"), (151, "100"),
    ], 1);
    let msg_b = fix::fix_build(&[
        (35, "8"), (11, "71"), (17, "EXEC_B1"),
        (39, "2"), (150, "F"),
        (31, "150.0"), (32, "100"), (151, "0"),
    ], 2);
    engine.inject_ccp_message(&msg_a);
    engine.inject_ccp_message(&msg_b);

    assert_eq!(shared.orders.drain_fills().len(), 2);
    assert_eq!(engine.context_mut().position(0), 200);
}

// ═══════════════════════════════════════════════════════════════════════
//  EDGE CASES — 256-instrument boundary
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn register_255_instruments_succeeds() {
    let shared = Arc::new(SharedState::new());
    let mut engine = HotLoop::new(shared.clone(), None, None);
    for i in 0..256 {
        let id = engine.context_mut().register_instrument(i as i64 + 1000);
        assert_eq!(id, i as u32);
    }
}

#[test]
#[should_panic(expected = "too many instruments")]
fn register_257th_instrument_panics() {
    let shared = Arc::new(SharedState::new());
    let mut engine = HotLoop::new(shared.clone(), None, None);
    for i in 0..257 {
        engine.context_mut().register_instrument(i as i64 + 1000);
    }
}

#[test]
fn register_same_instrument_twice_returns_same_id() {
    let shared = Arc::new(SharedState::new());
    let mut engine = HotLoop::new(shared.clone(), None, None);
    let id1 = engine.context_mut().register_instrument(265598);
    let id2 = engine.context_mut().register_instrument(265598);
    assert_eq!(id1, id2);
}

// ═══════════════════════════════════════════════════════════════════════
//  EDGE CASES — quote values
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn zero_price_quote_dispatches_correctly() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);

    let q = Quote { bid: 0, ask: 0, last: 0, bid_size: 0, ask_size: 0,
        last_size: 0, high: 0, low: 0, volume: 0, close: 0, open: 0, timestamp_ns: 0 };
    shared.market.push_quote(0, &q);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    // Zero quotes should not cause panic — they just dispatch as 0.0
    // (Whether they dispatch depends on change detection — default is [0;12])
}

#[test]
fn crossed_market_quote_dispatches() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);

    // Crossed: ask < bid (happens during fast markets)
    let q = Quote {
        bid: 151 * PRICE_SCALE, ask: 150 * PRICE_SCALE,
        last: 0, bid_size: 0, ask_size: 0, last_size: 0,
        high: 0, low: 0, volume: 0, close: 0, open: 0, timestamp_ns: 0,
    };
    shared.market.push_quote(0, &q);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    // Should dispatch normally — no validation (that's user's job)
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:151")));
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:2:150")));
}

#[test]
fn negative_price_quote_dispatches() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);

    // Negative prices (valid for some instruments like spreads)
    let q = Quote {
        bid: -5 * PRICE_SCALE, ask: -4 * PRICE_SCALE,
        last: 0, bid_size: 0, ask_size: 0, last_size: 0,
        high: 0, low: 0, volume: 0, close: 0, open: 0, timestamp_ns: 0,
    };
    shared.market.push_quote(0, &q);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:-5")));
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:2:-4")));
}

// ═══════════════════════════════════════════════════════════════════════
//  EDGE CASES — empty responses
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn empty_historical_data_response() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_data(5, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![], // empty
        is_complete: true,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    // Should just get historical_data_end, no bars
    assert!(!w.events.iter().any(|e| e.starts_with("historical_data:5:")));
    assert!(w.events.iter().any(|e| e == "historical_data_end:5"));
}

#[test]
fn empty_scanner_results() {
    use ibx::control::scanner::ScannerResult;
    let (client, _rx, shared) = test_client();
    shared.reference.push_scanner_data(3, ScannerResult {
        con_ids: vec![],
        scan_time: "2026-03-13".into(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("scanner_data:3:")));
    assert!(w.events.iter().any(|e| e == "scanner_data_end:3"));
}

#[test]
fn empty_matching_symbols() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_matching_symbols(8, vec![]);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "symbol_samples:8:0"));
}

#[test]
fn empty_historical_news() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_news(4, vec![], true);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "historical_news_end:4:true"));
}

// ═══════════════════════════════════════════════════════════════════════
//  EDGE CASES — process_msgs idempotency
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_multiple_rapid_calls_no_duplicates() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 1, side: Side::Buy,
        price: PRICE_SCALE, qty: 1, remaining: 0,
        commission: 0, timestamp_ns: 0,
    });

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let count1 = w.events.iter().filter(|e| e.starts_with("order_status:1:")).count();
    assert_eq!(count1, 1);

    // Second call — should have no fills
    w.events.clear();
    client.process_msgs(&mut w);
    let count2 = w.events.iter().filter(|e| e.starts_with("order_status:1:")).count();
    assert_eq!(count2, 0);
}

// ═══════════════════════════════════════════════════════════════════════
//  CONCURRENCY — SeqLock quote reads during writes
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn concurrent_seqlock_quote_read_write() {
    let shared = Arc::new(SharedState::new());
    let writer_shared = shared.clone();
    let reader_shared = shared.clone();

    let writer = thread::spawn(move || {
        for i in 0..10_000i64 {
            let mut q = Quote::default();
            q.bid = i * PRICE_SCALE;
            q.ask = (i + 1) * PRICE_SCALE;
            q.last = i * PRICE_SCALE;
            writer_shared.market.push_quote(0, &q);
        }
    });

    let reader = thread::spawn(move || {
        for _ in 0..10_000 {
            let q = reader_shared.market.quote(0);
            // Consistency check: if bid is set, ask should be bid + PRICE_SCALE
            if q.bid > 0 {
                assert_eq!(q.ask, q.bid + PRICE_SCALE,
                    "SeqLock inconsistency: bid={}, ask={}", q.bid, q.ask);
            }
        }
    });

    writer.join().unwrap();
    reader.join().unwrap();
}

#[test]
fn concurrent_seqlock_multiple_readers() {
    let shared = Arc::new(SharedState::new());
    // Pre-write a known quote
    let mut q = Quote::default();
    q.bid = 100 * PRICE_SCALE;
    q.ask = 101 * PRICE_SCALE;
    shared.market.push_quote(0, &q);

    let handles: Vec<_> = (0..4).map(|_| {
        let s = shared.clone();
        thread::spawn(move || {
            for _ in 0..5_000 {
                let q = s.market.quote(0);
                assert!(q.bid == 0 || q.bid == 100 * PRICE_SCALE);
            }
        })
    }).collect();

    for h in handles {
        h.join().unwrap();
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  CONCURRENCY — quote_by_instrument from multiple threads
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn concurrent_quote_by_instrument() {
    let shared = Arc::new(SharedState::new());
    let (tx, _rx) = crossbeam_channel::unbounded();
    let handle = thread::spawn(|| {});
    let client = Arc::new(EClient::from_parts(shared.clone(), tx, handle, "DU123".into()));

    // Write quote
    let mut q = Quote::default();
    q.bid = 200 * PRICE_SCALE;
    shared.market.push_quote(0, &q);

    let handles: Vec<_> = (0..4).map(|_| {
        let c = client.clone();
        thread::spawn(move || {
            for _ in 0..5_000 {
                let q = c.quote_by_instrument(0);
                assert!(q.bid == 0 || q.bid == 200 * PRICE_SCALE);
            }
        })
    }).collect();

    for h in handles {
        h.join().unwrap();
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  CONCURRENCY — disconnect while process_msgs runs
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn concurrent_disconnect_during_process_msgs() {
    let shared = Arc::new(SharedState::new());
    let (tx, _rx) = crossbeam_channel::unbounded();
    let handle = thread::spawn(|| {});
    let client = Arc::new(EClient::from_parts(shared.clone(), tx, handle, "DU123".into()));

    // Push lots of data
    for i in 0..100 {
        shared.orders.push_fill(Fill {
            instrument: 0, order_id: i, side: Side::Buy,
            price: PRICE_SCALE, qty: 1, remaining: 0,
            commission: 0, timestamp_ns: 0,
        });
    }

    let client_process = client.clone();
    let process_thread = thread::spawn(move || {
        let mut w = RecordingWrapper::default();
        client_process.process_msgs(&mut w);
        w.events.len()
    });

    // Disconnect from main thread
    client.disconnect();

    // Should not panic
    let count = process_thread.join().unwrap();
    assert!(count > 0);
}

// ═══════════════════════════════════════════════════════════════════════
//  CONCURRENCY — rapid subscribe/unsubscribe
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn rapid_subscribe_unsubscribe_no_stale_state() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);

    for _ in 0..100 {
        let _ = client.req_mkt_data(1, &spy(), "", false, false);
        client.cancel_mkt_data(1).unwrap();
    }

    // All commands should have been sent without panic
    let mut count = 0;
    while rx.try_recv().is_ok() {
        count += 1;
    }
    assert!(count > 0);

    // After all subscribe/unsubscribe cycles, mapping should be cleared
    let mut w = RecordingWrapper::default();
    let mut q = Quote::default();
    q.bid = 999 * PRICE_SCALE;
    shared.market.push_quote(0, &q);
    client.process_msgs(&mut w);
    // No ticks should arrive since all subscriptions were cancelled
    let ticks: Vec<_> = w.events.iter().filter(|e| e.starts_with("tick_price:1:")).collect();
    assert!(ticks.is_empty(), "no ticks after final unsubscribe");
}

// ═══════════════════════════════════════════════════════════════════════
//  CONCURRENCY — concurrent place_order and process_msgs
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn concurrent_place_order_and_process_msgs() {
    let shared = Arc::new(SharedState::new());
    shared.market.set_instrument_count(1);
    let (tx, _rx) = crossbeam_channel::unbounded();
    let handle = thread::spawn(|| {});
    let client = Arc::new(EClient::from_parts(shared.clone(), tx, handle, "DU123".into()));

    // Thread A: process_msgs
    let client_a = client.clone();
    let shared_a = shared.clone();
    let process_handle = thread::spawn(move || {
        for i in 0..50 {
            shared_a.orders.push_fill(Fill {
                instrument: 0, order_id: i, side: Side::Buy,
                price: PRICE_SCALE, qty: 1, remaining: 0,
                commission: 0, timestamp_ns: 0,
            });
            let mut w = RecordingWrapper::default();
            client_a.process_msgs(&mut w);
        }
    });

    // Thread B: place_order
    let client_b = client.clone();
    let order_handle = thread::spawn(move || {
        for _ in 0..50 {
            let order = Order {
                action: "BUY".into(), total_quantity: 1.0,
                order_type: "MKT".into(), ..Default::default()
            };
            let _ = client_b.place_order(0, &Contract { con_id: 756733, symbol: "SPY".into(), ..Default::default() }, &order);
        }
    });

    // Both should complete without panic or deadlock
    process_handle.join().unwrap();
    order_handle.join().unwrap();
}

// ═══════════════════════════════════════════════════════════════════════
//  CONCURRENCY — concurrent account reads
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn concurrent_account_read_write() {
    let shared = Arc::new(SharedState::new());
    let writer_shared = shared.clone();

    let writer = thread::spawn(move || {
        for i in 0..5_000i64 {
            let mut a = AccountState::default();
            a.net_liquidation = i * PRICE_SCALE;
            a.buying_power = i * 2 * PRICE_SCALE;
            writer_shared.portfolio.set_account(&a);
        }
    });

    let reader = thread::spawn(move || {
        for _ in 0..5_000 {
            let a = shared.portfolio.account();
            // buying_power should always be 2x net_liquidation
            if a.net_liquidation > 0 {
                assert_eq!(a.buying_power, a.net_liquidation * 2,
                    "Account inconsistency: net_liq={}, buying_power={}",
                    a.net_liquidation, a.buying_power);
            }
        }
    });

    writer.join().unwrap();
    reader.join().unwrap();
}

// ═══════════════════════════════════════════════════════════════════════
//  EDGE CASES — OrderBuffer
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn order_buffer_push_drain_cycle() {
    let mut buf = OrderBuffer::new();
    for i in 0..64 {
        buf.push(OrderRequest::SubmitMarket {
            order_id: i, instrument: 0, side: Side::Buy, qty: 1,
        });
    }
    let drained: Vec<_> = buf.drain().collect();
    assert_eq!(drained.len(), 64);
    // After drain, buffer should be empty for reuse
    assert_eq!(buf.drain().count(), 0);
}

#[test]
fn order_buffer_multiple_drain_cycles() {
    let mut buf = OrderBuffer::new();
    for cycle in 0..5 {
        for i in 0..10 {
            buf.push(OrderRequest::SubmitMarket {
                order_id: cycle * 10 + i, instrument: 0, side: Side::Buy, qty: 1,
            });
        }
        let drained: Vec<_> = buf.drain().collect();
        assert_eq!(drained.len(), 10);
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  EDGE CASES — SharedState drain idempotency
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn shared_state_all_drains_empty_after_first_call() {
    let ss = SharedState::new();

    // Push one item to each queue
    ss.orders.push_fill(Fill { instrument: 0, order_id: 1, side: Side::Buy,
        price: PRICE_SCALE, qty: 1, remaining: 0, commission: 0, timestamp_ns: 0 });
    ss.orders.push_order_update(OrderUpdate { order_id: 1, instrument: 0,
        status: OrderStatus::Filled, filled_qty: 1, remaining_qty: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0 });
    ss.orders.push_cancel_reject(CancelReject { order_id: 1, instrument: 0,
        reject_type: 1, reason_code: 0, timestamp_ns: 0 });
    ss.market.push_tbt_trade(TbtTrade { instrument: 0, price: PRICE_SCALE,
        size: 1, timestamp: 0, exchange: String::new(), conditions: String::new() });
    ss.market.push_tbt_quote(TbtQuote { instrument: 0, bid: PRICE_SCALE, ask: PRICE_SCALE,
        bid_size: 1, ask_size: 1, timestamp: 0 });

    // First drain
    assert_eq!(ss.orders.drain_fills().len(), 1);
    assert_eq!(ss.orders.drain_order_updates().len(), 1);
    assert_eq!(ss.orders.drain_cancel_rejects().len(), 1);
    assert_eq!(ss.market.drain_tbt_trades().len(), 1);
    assert_eq!(ss.market.drain_tbt_quotes().len(), 1);

    // Second drain — all empty
    assert!(ss.orders.drain_fills().is_empty());
    assert!(ss.orders.drain_order_updates().is_empty());
    assert!(ss.orders.drain_cancel_rejects().is_empty());
    assert!(ss.market.drain_tbt_trades().is_empty());
    assert!(ss.market.drain_tbt_quotes().is_empty());
}

// ═══════════════════════════════════════════════════════════════════════
//  CONCURRENCY — concurrent drain from multiple threads
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn concurrent_drain_fills_no_duplicates() {
    let shared = Arc::new(SharedState::new());

    // Push 100 fills
    for i in 0..100 {
        shared.orders.push_fill(Fill {
            instrument: 0, order_id: i, side: Side::Buy,
            price: PRICE_SCALE, qty: 1, remaining: 0,
            commission: 0, timestamp_ns: 0,
        });
    }

    // Two threads race to drain
    let s1 = shared.clone();
    let s2 = shared.clone();

    let h1 = thread::spawn(move || s1.orders.drain_fills().len());
    let h2 = thread::spawn(move || s2.orders.drain_fills().len());

    let count1 = h1.join().unwrap();
    let count2 = h2.join().unwrap();

    // Total should be exactly 100 — no duplicates, no lost fills
    assert_eq!(count1 + count2, 100, "Total fills should be 100, got {} + {}", count1, count2);
}
