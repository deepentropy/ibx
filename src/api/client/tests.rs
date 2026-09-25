use std::sync::Arc;

use super::*;
use crate::api::types::PRICE_SCALE_F;
use crate::api::wrapper::Wrapper;
use crate::api::wrapper::tests::RecordingWrapper;
use crate::bridge::SharedState;
use crate::control::historical::{HistoricalResponse, HistoricalBar, HeadTimestampResponse};
use crate::control::contracts::{ContractDefinition, SecurityType, SymbolMatch};
use crate::control::scanner::{ScannerEntry, ScannerResult};
use crate::control::news::NewsHeadline;
use crate::control::histogram::HistogramEntry;

// ibx#399: the gateway leaves host and credentials empty for the caller, and
// the Rust client never filled them, so every auto-reconnect was skipped.
#[test]
fn connect_caches_reconnect_credentials() {
    let mut hot_loop = crate::engine::hot_loop::HotLoop::new(Arc::new(SharedState::new()), None, None);
    // What into_hot_loop_with_farms installs: session fields set, caller fields empty.
    hot_loop.set_reconnect_auth(crate::gateway::ReconnectAuth {
        host: String::new(),
        username: String::new(),
        password: zeroize::Zeroizing::new(String::new()),
        paper: false,
        session_key: num_bigint::BigUint::default(),
        session_token: num_bigint::BigUint::default(),
        server_session_id: String::new(),
        hw_info: String::new(),
        encoded: String::new(),
        hmds_host: String::new(),
        hmds_farm: String::new(),
    });
    assert!(!hot_loop.has_reconnect_host(), "gateway leaves the host empty");

    let config = EClientConfig {
        username: "user".into(),
        password: "pass".into(),
        host: "gw.example".into(),
        paper: true,
        core_id: None,
    };
    cache_reconnect_credentials(&mut hot_loop, &config);
    assert!(hot_loop.has_reconnect_host());
}

/// Helper: create a test EClient backed by SharedState + channel.
fn test_client() -> (EClient, crossbeam_channel::Receiver<ControlCommand>, Arc<SharedState>) {
    let shared = Arc::new(SharedState::new());
    let (tx, rx) = crossbeam_channel::unbounded();
    let handle = std::thread::spawn(|| {});
    let client = EClient::from_parts(shared.clone(), tx, handle, "DU123".into());
    // Pre-seed SPY so find_or_register_instrument hits the fast path.
    client.core.con_id_to_instrument.lock().unwrap().insert(756733, 0);
    (client, rx, shared)
}

/// Helper: SPY contract.
fn spy() -> Contract {
    Contract { con_id: 756733, symbol: "SPY".into(), ..Default::default() }
}

// ═══════════════════════════════════════════════════════════════════
//  Algo parsing
// ═══════════════════════════════════════════════════════════════════

#[test]
fn parse_algo_vwap() {
    let params = vec![
        TagValue { tag: "maxPctVol".into(), value: "0.1".into() },
        TagValue { tag: "startTime".into(), value: "09:30:00".into() },
        TagValue { tag: "endTime".into(), value: "16:00:00".into() },
    ];
    let algo = parse_algo_params("vwap", &params).unwrap();
    match algo {
        AlgoParams::Vwap { max_pct_vol, start_time, end_time, .. } => {
            assert!((max_pct_vol - 0.1).abs() < 1e-10);
            assert_eq!(start_time, "09:30:00");
            assert_eq!(end_time, "16:00:00");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_algo_twap() {
    let algo = parse_algo_params("twap", &[]).unwrap();
    assert!(matches!(algo, AlgoParams::Twap { .. }));
}

#[test]
fn parse_algo_arrival_price() {
    let params = vec![
        TagValue { tag: "maxPctVol".into(), value: "0.25".into() },
        TagValue { tag: "riskAversion".into(), value: "Aggressive".into() },
    ];
    let algo = parse_algo_params("arrivalpx", &params).unwrap();
    match algo {
        AlgoParams::ArrivalPx { max_pct_vol, risk_aversion, .. } => {
            assert!((max_pct_vol - 0.25).abs() < 1e-10);
            assert!(matches!(risk_aversion, RiskAversion::Aggressive));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_algo_close_price() {
    let algo = parse_algo_params("closepx", &[]).unwrap();
    assert!(matches!(algo, AlgoParams::ClosePx { .. }));
}

#[test]
fn parse_algo_dark_ice() {
    let params = vec![
        TagValue { tag: "displaySize".into(), value: "200".into() },
    ];
    let algo = parse_algo_params("darkice", &params).unwrap();
    match algo {
        AlgoParams::DarkIce { display_size, .. } => assert_eq!(display_size, 200),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_algo_pct_vol() {
    let params = vec![
        TagValue { tag: "pctVol".into(), value: "0.05".into() },
    ];
    let algo = parse_algo_params("pctvol", &params).unwrap();
    match algo {
        AlgoParams::PctVol { pct_vol, .. } => assert!((pct_vol - 0.05).abs() < 1e-10),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_algo_unsupported() {
    assert!(parse_algo_params("unknown", &[]).is_err());
}

// ═══════════════════════════════════════════════════════════════════
//  Connection
// ═══════════════════════════════════════════════════════════════════

#[test]
fn is_connected_after_construction() {
    let (client, _rx, _shared) = test_client();
    assert!(client.is_connected());
}

#[test]
fn disconnect_sends_shutdown_and_clears_connected() {
    let (client, rx, _shared) = test_client();
    client.disconnect();
    assert!(!client.is_connected());
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Shutdown));
}

#[test]
fn disconnect_idempotent() {
    let (client, _rx, _shared) = test_client();
    client.disconnect();
    client.disconnect();
    assert!(!client.is_connected());
}

// ═══════════════════════════════════════════════════════════════════
//  next_order_id / req_ids
// ═══════════════════════════════════════════════════════════════════

#[test]
fn next_order_id_monotonic() {
    let (client, _rx, _shared) = test_client();
    let id1 = client.next_order_id();
    let id2 = client.next_order_id();
    let id3 = client.next_order_id();
    assert!(id2 > id1);
    assert!(id3 > id2);
}

#[test]
fn req_ids_calls_wrapper() {
    let (client, _rx, _shared) = test_client();
    let mut w = RecordingWrapper::default();
    client.req_ids(&mut w);
    assert_eq!(w.events.len(), 1);
    assert!(w.events[0].starts_with("next_valid_id:"));
}

// ═══════════════════════════════════════════════════════════════════
//  Market data requests
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_mkt_data_sends_register_and_subscribe() {
    let (client, rx, _shared) = test_client();
    let _ = client.req_mkt_data(1, &spy(), "", false, false);
    let cmd1 = rx.try_recv().unwrap();
    assert!(matches!(cmd1, ControlCommand::RegisterInstrument { con_id: 756733, .. }));
    let cmd2 = rx.try_recv().unwrap();
    match cmd2 {
        ControlCommand::Subscribe { con_id, symbol, .. } => {
            assert_eq!(con_id, 756733);
            assert_eq!(symbol, "SPY");
        }
        _ => panic!("expected Subscribe, got {:?}", cmd2),
    }
}

#[test]
fn req_mkt_data_defaults_to_realtime_mode() {
    let (client, rx, _shared) = test_client();
    let _ = client.req_mkt_data(1, &spy(), "", false, false);
    let _register = rx.try_recv().unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Subscribe { mode_9887, .. } => assert_eq!(mode_9887, 0),
        other => panic!("expected Subscribe, got {:?}", other),
    }
}

#[test]
fn req_mkt_data_ex_propagates_mode_9887() {
    for mode in [1_i32, 2, 3] {
        let (client, rx, _shared) = test_client();
        let _ = client.req_mkt_data_ex(1, &spy(), "", false, false, mode);
        let _register = rx.try_recv().unwrap();
        match rx.try_recv().unwrap() {
            ControlCommand::Subscribe { mode_9887, con_id, .. } => {
                assert_eq!(mode_9887, mode);
                assert_eq!(con_id, 756733);
            }
            other => panic!("expected Subscribe, got {:?}", other),
        }
    }
}

// ibx#233: a second live subscription on the same contract would clobber
// the first's reverse mapping and orphan it silently. Reject at the call.
#[test]
fn req_mkt_data_duplicate_instrument_is_rejected() {
    let (client, rx, _shared) = test_client();
    // Existing live subscription for SPY (instrument 0) under req_id 1.
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);

    let err = client.req_mkt_data(2, &spy(), "", false, false).unwrap_err();
    assert!(err.contains("req_id 1"), "got: {}", err);
    assert!(rx.try_recv().is_err(), "nothing may reach the engine");
}

#[test]
fn cancel_mkt_data_sends_unsubscribe() {
    let (client, rx, _shared) = test_client();
    // Pre-register mapping
    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    client.cancel_mkt_data(1).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Unsubscribe { instrument: 0 }));
    // Mapping should be cleared
    assert!(client.core.req_to_instrument.lock().unwrap().get(&1).is_none());
}

#[test]
fn cancel_mkt_data_unknown_req_id_no_panic() {
    let (client, rx, _shared) = test_client();
    client.cancel_mkt_data(999).unwrap();
    assert!(rx.try_recv().is_err()); // no commands sent
}

#[test]
fn req_tick_by_tick_data_sends_subscribe_tbt() {
    let (client, rx, _shared) = test_client();
    let _ = client.req_tick_by_tick_data(10, &spy(), "BidAsk", 0, false);
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::SubscribeTbt { con_id, symbol, tbt_type, .. } => {
            assert_eq!(con_id, 756733);
            assert_eq!(symbol, "SPY");
            assert!(matches!(tbt_type, TbtType::BidAsk));
        }
        _ => panic!("expected SubscribeTbt"),
    }
}

#[test]
fn req_tick_by_tick_data_defaults_to_last() {
    let (client, rx, _shared) = test_client();
    let _ = client.req_tick_by_tick_data(10, &spy(), "AllLast", 0, false);
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::SubscribeTbt { tbt_type, .. } => {
            assert!(matches!(tbt_type, TbtType::Last));
        }
        _ => panic!("expected SubscribeTbt"),
    }
}

#[test]
fn cancel_tick_by_tick_data_sends_unsubscribe_tbt() {
    let (client, rx, _shared) = test_client();
    client.core.req_to_instrument.lock().unwrap().insert(10, 3);
    client.cancel_tick_by_tick_data(10).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::UnsubscribeTbt { instrument: 3 }));
}

#[test]
fn cancel_tick_by_tick_unknown_req_id_no_panic() {
    let (client, rx, _shared) = test_client();
    client.cancel_tick_by_tick_data(999).unwrap();
    assert!(rx.try_recv().is_err());
}

// ═══════════════════════════════════════════════════════════════════
//  Orders — every order type
// ═══════════════════════════════════════════════════════════════════

#[test]
fn place_order_market() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order { action: "BUY".into(), total_quantity: 100.0, order_type: "MKT".into(), ..Default::default() };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitMarket { qty, .. }) => assert_eq!(qty, 100),
        _ => panic!("expected SubmitMarket, got {:?}", cmd),
    }
}

#[test]
fn place_order_limit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 50.0, order_type: "LMT".into(),
        lmt_price: 150.25, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitLimit { qty, price, .. }) => {
            assert_eq!(qty, 50);
            assert_eq!(price, (150.25 * PRICE_SCALE_F) as i64);
        }
        _ => panic!("expected SubmitLimit, got {:?}", cmd),
    }
}

#[test]
fn place_order_trailing_stop_carries_initial_trigger() {
    // ibx#225 Part B / ib-agent#173: a plain amount trailing stop can carry an
    // initial stop trigger (trailStopPrice); it must reach the request.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL".into(),
        aux_price: 0.50,             // trail amount
        trail_stop_price: 10.00,     // initial stop trigger
        ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitTrailingStop { trail_amt, trail_stop_price, .. }) => {
            assert_eq!(trail_amt, (0.50 * PRICE_SCALE_F) as i64);
            assert_eq!(trail_stop_price, (10.00 * PRICE_SCALE_F) as i64);
        }
        cmd => panic!("expected SubmitTrailingStop, got {:?}", cmd),
    }
}

#[test]
fn place_order_trailing_stop_without_trigger_is_unset() {
    // Default (f64::MAX) must encode as 0 (not set), so the tag is omitted.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL".into(),
        aux_price: 0.50, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitTrailingStop { trail_stop_price, .. }) => {
            assert_eq!(trail_stop_price, 0);
        }
        cmd => panic!("expected SubmitTrailingStop, got {:?}", cmd),
    }
}

#[test]
fn place_order_adjustable_trail_carries_trailing_amount_and_unit() {
    // ibx#225 / ib-agent#167: a base STP that converts to a TRAIL must carry
    // the trailing amount and unit through to the SubmitAdjustableStop request.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(),
        aux_price: 11.00,                          // base stop price
        adjusted_order_type: "TRAIL".into(),
        trigger_price: 11.00,
        adjusted_stop_price: 10.00,
        adjusted_trailing_amount: 0.50,
        adjustable_trailing_unit: 0,               // amount
        ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitAdjustableStop {
            adjusted_order_type, stop_price, trigger_price, adjusted_stop_price,
            adjusted_trailing_amount, adjustable_trailing_unit, .. }) => {
            assert_eq!(adjusted_order_type, crate::types::AdjustedOrderType::Trail);
            assert_eq!(stop_price, (11.00 * PRICE_SCALE_F) as i64);
            assert_eq!(trigger_price, (11.00 * PRICE_SCALE_F) as i64);
            assert_eq!(adjusted_stop_price, (10.00 * PRICE_SCALE_F) as i64);
            assert_eq!(adjusted_trailing_amount, (0.50 * PRICE_SCALE_F) as i64);
            assert_eq!(adjustable_trailing_unit, 0);
        }
        _ => panic!("expected SubmitAdjustableStop, got {:?}", cmd),
    }
}

// ibx#240: an adjustable stop with a parent, an OCA group or a non-DAY tif
// must take the extended path, or a bracket child ships unlinked and DAY.
#[test]
fn place_order_adjustable_stop_child_keeps_parent_oca_and_tif() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(),
        aux_price: 9.00,
        adjusted_order_type: "STP".into(),
        trigger_price: 11.00,
        adjusted_stop_price: 10.00,
        parent_id: 100,
        oca_group: "BR1".into(),
        tif: "GTC".into(),
        ..Default::default()
    };
    client.place_order(101, &spy(), &order).unwrap();

    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitEx { kind, tif, attrs, .. }) => {
            match kind {
                crate::types::OrderKind::AdjustableStop {
                    stop_price, trigger_price, adjusted_order_type, adjusted_stop_price, .. } => {
                    assert_eq!(adjusted_order_type, crate::types::AdjustedOrderType::Stop);
                    assert_eq!(stop_price, (9.00 * PRICE_SCALE_F) as i64);
                    assert_eq!(trigger_price, (11.00 * PRICE_SCALE_F) as i64);
                    assert_eq!(adjusted_stop_price, (10.00 * PRICE_SCALE_F) as i64);
                }
                other => panic!("expected AdjustableStop kind, got {:?}", other),
            }
            assert_eq!(tif, b'1');
            assert_eq!(attrs.parent_id, 100);
            assert_eq!(attrs.oca_group_str, "BR1");
        }
        cmd => panic!("expected SubmitEx, got {:?}", cmd),
    }
}

#[test]
fn place_order_adjustable_trail_percent_unit_passes_through() {
    // Percent unit (100) must survive; the trailing amount is a percent value.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(),
        aux_price: 11.00,
        adjusted_order_type: "TRAIL".into(),
        adjusted_trailing_amount: 1.00,            // 1.00%
        adjustable_trailing_unit: 100,             // percent
        ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitAdjustableStop {
            adjustable_trailing_unit, adjusted_trailing_amount, .. }) => {
            assert_eq!(adjustable_trailing_unit, 100);
            assert_eq!(adjusted_trailing_amount, (1.00 * PRICE_SCALE_F) as i64);
        }
        cmd => panic!("expected SubmitAdjustableStop, got {:?}", cmd),
    }
}

#[test]
fn place_order_limit_gtc_uses_limit_ex() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 10.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "GTC".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitLimitEx { tif, .. }) => {
            assert_eq!(tif, b'1'); // GTC
        }
        _ => panic!("expected SubmitLimitEx, got {:?}", cmd),
    }
}

#[test]
fn place_order_limit_hidden_uses_limit_ex() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 10.0, order_type: "LMT".into(),
        lmt_price: 100.0, hidden: true, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitLimitEx { attrs, .. }) => {
            assert!(attrs.hidden);
        }
        _ => panic!("expected SubmitLimitEx, got {:?}", cmd),
    }
}

// ── ibx#224: every order type must carry attrs + tif when set ──

#[test]
fn place_order_stop_with_parent_and_gtc_uses_submit_ex() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(),
        aux_price: 240.0, tif: "GTC".into(), parent_id: 42,
        oca_group: "77".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { kind, tif, attrs, .. }) => {
            assert!(matches!(kind, crate::types::OrderKind::Stop { stop_price }
                if stop_price == (240.0 * PRICE_SCALE_F) as i64));
            assert_eq!(tif, b'1'); // GTC
            assert_eq!(attrs.parent_id, 42);
            assert_eq!(attrs.oca_group, 77);
        }
        _ => panic!("expected SubmitEx, got {:?}", cmd),
    }
}

#[test]
fn place_order_market_outside_rth_uses_submit_ex() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "MKT".into(),
        outside_rth: true, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { kind, tif, attrs, .. }) => {
            assert!(matches!(kind, crate::types::OrderKind::Market));
            assert_eq!(tif, b'0'); // DAY
            assert!(attrs.outside_rth);
        }
        _ => panic!("expected SubmitEx, got {:?}", cmd),
    }
}

#[test]
fn place_order_trailing_amount_with_oca_uses_submit_ex() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL".into(),
        aux_price: 2.0, tif: "GTC".into(), oca_group: "exit_9".into(),
        oca_type: 2, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { kind, tif, attrs, .. }) => {
            assert!(matches!(kind, crate::types::OrderKind::TrailingStop { trail_amt, .. }
                if trail_amt == (2.0 * PRICE_SCALE_F) as i64));
            assert_eq!(tif, b'1');
            assert_eq!(attrs.oca_group_str, "exit_9");
            assert_eq!(attrs.oca_type, 2); // ibx#215
        }
        _ => panic!("expected SubmitEx, got {:?}", cmd),
    }
}

#[test]
fn place_order_empty_tif_stays_plain() {
    // tif "" is DAY (the official API default) — no extended routing.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "STP".into(),
        aux_price: 240.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();
    assert!(matches!(rx.try_recv().unwrap(),
        ControlCommand::Order(OrderRequest::SubmitStop { .. })));
}

// ── ibx#226: transmit=false must be rejected, not silently ignored ──

#[test]
fn place_order_transmit_false_is_rejected() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, transmit: false, ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.to_string().contains("transmit=false"), "got: {}", err);
    assert!(rx.try_recv().is_err(), "nothing may reach the engine");
}

#[test]
fn place_order_unknown_tif_is_rejected() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "GTX".into(), ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.to_string().contains("tif"), "got: {}", err);
    assert!(rx.try_recv().is_err());
}

#[test]
fn place_order_all_or_none_trail_is_rejected() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL".into(),
        aux_price: 2.0, all_or_none: true, ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.to_string().contains("all_or_none"), "got: {}", err);
    assert!(rx.try_recv().is_err());
}

// ── ibx#215: oca_type carried and coerced ──

#[test]
fn attrs_oca_type_coerces_out_of_range_to_unset() {
    let order = Order { oca_type: 9, ..Default::default() };
    assert_eq!(order.attrs().oca_type, 0);
    let order = Order { oca_type: 4, ..Default::default() };
    assert_eq!(order.attrs().oca_type, 4);
    let order = Order { oca_type: -1, ..Default::default() };
    assert_eq!(order.attrs().oca_type, 0);
}

#[test]
fn place_order_stop() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP".into(),
        aux_price: 145.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitStop { side, stop_price, .. }) => {
            assert!(matches!(side, Side::Sell));
            assert_eq!(stop_price, (145.0 * PRICE_SCALE_F) as i64);
        }
        _ => panic!("expected SubmitStop, got {:?}", cmd),
    }
}

#[test]
fn place_order_stop_limit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP LMT".into(),
        lmt_price: 144.0, aux_price: 145.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitStopLimit { price, stop_price, .. }) => {
            assert_eq!(price, (144.0 * PRICE_SCALE_F) as i64);
            assert_eq!(stop_price, (145.0 * PRICE_SCALE_F) as i64);
        }
        _ => panic!("expected SubmitStopLimit, got {:?}", cmd),
    }
}

#[test]
fn place_order_trailing_stop_amount() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "TRAIL".into(),
        aux_price: 2.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitTrailingStop { trail_amt, .. }) => {
            assert_eq!(trail_amt, (2.0 * PRICE_SCALE_F) as i64);
        }
        _ => panic!("expected SubmitTrailingStop, got {:?}", cmd),
    }
}

#[test]
fn place_order_trailing_stop_percent() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "TRAIL".into(),
        trailing_percent: 5.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitTrailingStopPct { trail_pct, .. }) => {
            assert_eq!(trail_pct, 500); // 5.0 * 100
        }
        _ => panic!("expected SubmitTrailingStopPct, got {:?}", cmd),
    }
}

#[test]
fn place_order_trailing_stop_limit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "TRAIL LIMIT".into(),
        lmt_price: 148.0, aux_price: 2.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitTrailingStopLimit { .. })));
}

#[test]
fn place_order_moc() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MOC".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitMoc { .. })));
}

#[test]
fn place_order_loc() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LOC".into(),
        lmt_price: 150.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitLoc { .. })));
}

#[test]
fn place_order_mit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MIT".into(),
        aux_price: 148.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitMit { .. })));
}

#[test]
fn place_order_lit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LIT".into(),
        lmt_price: 150.0, aux_price: 148.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitLit { .. })));
}

#[test]
fn place_order_mtl() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MTL".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitMtl { .. })));
}

#[test]
fn place_order_mkt_prt() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MKT PRT".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitMktPrt { .. })));
}

#[test]
fn place_order_stp_prt() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP PRT".into(),
        aux_price: 145.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitStpPrt { .. })));
}

#[test]
fn place_order_rel() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "REL".into(),
        aux_price: 0.10, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitRel { .. })));
}

#[test]
fn place_order_peg_mkt() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "PEG MKT".into(),
        aux_price: 0.05, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitPegMkt { .. })));
}

#[test]
fn place_order_peg_mid() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "PEG MID".into(),
        aux_price: 0.02, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitPegMid { .. })));
}

#[test]
fn place_order_midprice() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MIDPRICE".into(),
        lmt_price: 150.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitMidPrice { .. })));
}

#[test]
fn place_order_snap_mkt() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "SNAP MKT".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitSnapMkt { .. })));
}

#[test]
fn place_order_snap_mid() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "SNAP MID".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitSnapMid { .. })));
}

#[test]
fn place_order_snap_pri() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "SNAP PRI".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitSnapPri { .. })));
}

#[test]
fn place_order_box_top() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "BOX TOP".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitMtl { .. })));
}

#[test]
fn place_order_sell_side() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 50.0, order_type: "MKT".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitMarket { side, .. }) => {
            assert!(matches!(side, Side::Sell));
        }
        _ => panic!("expected SubmitMarket"),
    }
}

#[test]
fn place_order_short_sell_side() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SSHORT".into(), total_quantity: 50.0, order_type: "MKT".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitMarket { side, .. }) => {
            assert!(matches!(side, Side::ShortSell));
        }
        _ => panic!("expected SubmitMarket"),
    }
}

#[test]
fn place_order_algo_vwap() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1000.0, order_type: "LMT".into(),
        lmt_price: 150.0, algo_strategy: "vwap".into(),
        algo_params: vec![TagValue { tag: "maxPctVol".into(), value: "0.1".into() }],
        ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitAlgo { .. })));
}

// ibx#318: an algo bracket child keeps its parent link, OCA group and GTC.
#[test]
fn place_order_algo_bracket_child_keeps_parent_oca_and_tif() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 509.0,
        algo_strategy: "Twap".into(),
        algo_params: vec![TagValue { tag: "allowPastEndTime".into(), value: "1".into() }],
        parent_id: 100, oca_group: "BR1".into(), tif: "GTC".into(),
        ..Default::default()
    };
    client.place_order(101, &spy(), &order).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitAlgo { tif, attrs, .. }) => {
            assert_eq!(tif, b'1');
            assert_eq!(attrs.parent_id, 100);
            assert_eq!(attrs.oca_group_str, "BR1");
        }
        cmd => panic!("expected SubmitAlgo, got {:?}", cmd),
    }
    let adaptive = Order { algo_strategy: "Adaptive".into(), algo_params: vec![], ..order.clone() };
    client.place_order(102, &spy(), &adaptive).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitAdaptive { tif, attrs, .. }) => {
            assert_eq!(tif, b'1');
            assert_eq!(attrs.parent_id, 100);
        }
        cmd => panic!("expected SubmitAdaptive, got {:?}", cmd),
    }
}

// ibx#325: an order whose only extra is conditions took the plain path and
// was sent without them.
#[test]
fn place_order_with_only_conditions_keeps_them() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 237.0,
        conditions: vec![OrderCondition::Price {
            con_id: 265598, exchange: "SMART".into(), price: 509 * crate::types::PRICE_SCALE,
            is_more: true, trigger_method: 0,
        }],
        ..Default::default()
    };
    client.place_order(95, &spy(), &order).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitLimitEx { attrs, .. })
        | ControlCommand::Order(OrderRequest::SubmitEx { attrs, .. }) => {
            assert_eq!(attrs.conditions.len(), 1);
        }
        cmd => panic!("expected an extended submit carrying the conditions, got {:?}", cmd),
    }
}

#[test]
fn place_order_what_if() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, what_if: true, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitWhatIf { .. })));
}

#[test]
fn place_order_unsupported_type_returns_error() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "FANTASY".into(), ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Unsupported order type"));
}

#[test]
fn place_order_non_stk_contract_rejected() {
    // A non-STK contract must be rejected, not silently sent as a stock order
    // on the underlying. See: https://github.com/deepentropy/ibx/issues/202
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let opt = Contract { con_id: 999001, symbol: "AAPL".into(), sec_type: "OPT".into(), ..Default::default() };
    let order = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "MKT".into(), ..Default::default() };
    let result = client.place_order(1, &opt, &order);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("OPT"));
    assert!(err.contains("STK"));
    // No order must have been queued to the engine.
    assert!(rx.try_recv().is_err());
}

#[test]
fn place_order_explicit_stk_contract_accepted() {
    // An explicit sec_type="STK" must still be accepted.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let stk = Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), ..Default::default() };
    let order = Order { action: "BUY".into(), total_quantity: 100.0, order_type: "MKT".into(), ..Default::default() };
    client.place_order(1, &stk, &order).unwrap();
    assert!(rx.try_recv().is_ok());
}

#[test]
fn place_order_invalid_action_returns_error() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "INVALID".into(), total_quantity: 100.0, order_type: "MKT".into(), ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
}

#[test]
fn place_order_auto_assigns_id_when_zero() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MKT".into(), ..Default::default()
    };
    // order_id = 0 → auto-assign
    client.place_order(0, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitMarket { order_id, .. }) => {
            assert!(order_id > 0);
        }
        _ => panic!("expected SubmitMarket"),
    }
}

#[test]
fn cancel_order_sends_cancel_command() {
    let (client, rx, _shared) = test_client();
    client.cancel_order(42, "").unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::Cancel { order_id }) => assert_eq!(order_id, 42),
        _ => panic!("expected Cancel"),
    }
}

#[test]
fn req_global_cancel_sends_cancel_all_for_each_instrument() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(2);
    client.req_global_cancel().unwrap();
    let mut cancel_instruments = vec![];
    while let Ok(cmd) = rx.try_recv() {
        if let ControlCommand::Order(OrderRequest::CancelAll { instrument }) = cmd {
            cancel_instruments.push(instrument);
        }
    }
    assert_eq!(cancel_instruments.len(), 2);
    cancel_instruments.sort();
    assert_eq!(cancel_instruments, vec![0, 1]);
}

#[test]
fn req_global_cancel_no_instruments_no_commands() {
    let (client, rx, _shared) = test_client();
    client.req_global_cancel().unwrap();
    assert!(rx.try_recv().is_err());
}

// ═══════════════════════════════════════════════════════════════════
//  Order validation — aux_price guards (issue #115)
// ═══════════════════════════════════════════════════════════════════

#[test]
fn stp_order_with_zero_aux_price_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP".into(),
        lmt_price: 145.0, // common mistake: setting lmt_price instead of aux_price
        ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("aux_price"));
}

#[test]
fn stp_order_with_valid_aux_price_succeeds() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP".into(),
        aux_price: 145.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitStop { .. })));
}

#[test]
fn stp_lmt_order_with_zero_aux_price_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP LMT".into(),
        lmt_price: 144.0, ..Default::default() // aux_price missing
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("aux_price"));
}

#[test]
fn trail_order_with_zero_amount_and_zero_percent_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "TRAIL".into(),
        ..Default::default() // neither trailing_percent nor aux_price
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("trailing_percent"));
}

#[test]
fn trail_order_with_trailing_percent_succeeds() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "TRAIL".into(),
        trailing_percent: 5.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitTrailingStopPct { .. })));
}

#[test]
fn trail_limit_order_with_zero_aux_price_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "TRAIL LIMIT".into(),
        lmt_price: 148.0, ..Default::default() // aux_price missing
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("aux_price"));
}

#[test]
fn mit_order_with_zero_aux_price_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MIT".into(),
        ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("aux_price"));
}

#[test]
fn stp_prt_order_with_zero_aux_price_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP PRT".into(),
        ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("aux_price"));
}

#[test]
fn lit_order_with_zero_aux_price_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LIT".into(),
        lmt_price: 150.0, ..Default::default() // aux_price missing
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("aux_price"));
}

// ═══════════════════════════════════════════════════════════════════
//  Historical data requests
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_historical_data_sends_fetch_historical() {
    let (client, rx, _shared) = test_client();
    client.req_historical_data(5, &spy(), "20260101 16:00:00", "1 D", "1 hour", "TRADES", true, 1, false).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchHistorical { req_id, con_id, duration, bar_size, what_to_show, use_rth, .. } => {
            assert_eq!(req_id, 5);
            assert_eq!(con_id, 756733);
            assert_eq!(duration, "1 D");
            assert_eq!(bar_size, "1 hour");
            assert_eq!(what_to_show, "TRADES");
            assert!(use_rth);
        }
        _ => panic!("expected FetchHistorical"),
    }
}

// ── ibx#232: unknown bar_size / what_to_show reject instead of silently
// falling back to 5-minute / TRADES bars ──

#[test]
fn req_historical_data_rejects_unknown_bar_size() {
    let (client, rx, _shared) = test_client();
    // The issue's exact repro: "1 Min" (wrong case) used to return 5-minute
    // candles with no error.
    let err = client.req_historical_data(5, &spy(), "", "2 D", "1 Min", "TRADES", true, 1, false).unwrap_err();
    assert!(err.contains("bar_size"), "got: {}", err);
    assert!(rx.try_recv().is_err(), "nothing may reach the engine");
}

#[test]
fn req_historical_data_rejects_unknown_what_to_show() {
    let (client, rx, _shared) = test_client();
    let err = client.req_historical_data(5, &spy(), "", "2 D", "1 min", "TRADE", true, 1, false).unwrap_err();
    assert!(err.contains("what_to_show"), "got: {}", err);
    assert!(rx.try_recv().is_err());
}

#[test]
fn req_historical_data_rejects_unsupported_keep_up_to_date_size() {
    let (client, rx, _shared) = test_client();
    // "1 min" is valid on the batch path but not supported for streaming —
    // it used to silently downgrade to 5-minute bars on this path only.
    let err = client.req_historical_data(5, &spy(), "", "1 D", "1 min", "TRADES", true, 1, true).unwrap_err();
    assert!(err.contains("keep_up_to_date"), "got: {}", err);
    assert!(rx.try_recv().is_err());
}

#[test]
fn req_historical_data_accepts_streamable_keep_up_to_date_size() {
    let (client, rx, _shared) = test_client();
    client.req_historical_data(5, &spy(), "", "1 D", "5 mins", "TRADES", true, 1, true).unwrap();
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::FetchHistorical { keep_up_to_date: true, .. }));
}

#[test]
fn cancel_historical_data_sends_cancel() {
    let (client, rx, _shared) = test_client();
    client.cancel_historical_data(5).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::CancelHistorical { req_id: 5 }));
}

#[test]
fn req_head_time_stamp_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_head_time_stamp(10, &spy(), "TRADES", true, 1).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchHeadTimestamp { req_id, con_id, what_to_show, use_rth } => {
            assert_eq!(req_id, 10);
            assert_eq!(con_id, 756733);
            assert_eq!(what_to_show, "TRADES");
            assert!(use_rth);
        }
        _ => panic!("expected FetchHeadTimestamp"),
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Contract details
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_contract_details_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_contract_details(7, &spy()).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchContractDetails { req_id, con_id, .. } => {
            assert_eq!(req_id, 7);
            assert_eq!(con_id, 756733);
        }
        _ => panic!("expected FetchContractDetails"),
    }
}

#[test]
fn req_contract_details_forwards_filter_fields() {
    // ibx#229 / ib-agent#171: a by-symbol lookup must carry the disambiguation
    // filters (primary exchange, local symbol, expiry/strike/right, multiplier,
    // trading class) instead of dropping them.
    let (client, rx, _shared) = test_client();
    let contract = Contract {
        con_id: 0, symbol: "AAPL".into(), sec_type: "OPT".into(),
        exchange: "SMART".into(), currency: "USD".into(),
        primary_exchange: "NASDAQ".into(),
        local_symbol: "AAPL  260808C00250000".into(),
        last_trade_date_or_contract_month: "202608".into(),
        strike: 250.0,
        right: "C".into(),
        multiplier: "100".into(),
        trading_class: "AAPL".into(),
        ..Default::default()
    };
    client.req_contract_details(9, &contract).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::FetchContractDetails { req_id, con_id, filters, .. } => {
            assert_eq!(req_id, 9);
            assert_eq!(con_id, 0);
            assert_eq!(filters.primary_exchange, "NASDAQ");
            assert_eq!(filters.local_symbol, "AAPL  260808C00250000");
            assert_eq!(filters.last_trade_date_or_contract_month, "202608");
            assert_eq!(filters.strike, 250.0);
            assert_eq!(filters.right, "C");
            assert_eq!(filters.multiplier, "100");
            assert_eq!(filters.trading_class, "AAPL");
        }
        cmd => panic!("expected FetchContractDetails, got {:?}", cmd),
    }
}

#[test]
fn req_contract_details_forwards_identifier_lookup() {
    // ibx#229 / ib-agent#174: an identifier lookup (ISIN) must carry secId and
    // secIdType through to the fetch command.
    let (client, rx, _shared) = test_client();
    let contract = Contract {
        con_id: 0, sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(),
        sec_id: "US0378331005".into(), sec_id_type: "ISIN".into(),
        ..Default::default()
    };
    client.req_contract_details(11, &contract).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::FetchContractDetails { filters, .. } => {
            assert_eq!(filters.sec_id, "US0378331005");
            assert_eq!(filters.sec_id_type, "ISIN");
        }
        cmd => panic!("expected FetchContractDetails, got {:?}", cmd),
    }
}

#[test]
fn req_matching_symbols_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_matching_symbols(8, "AAPL").unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchMatchingSymbols { req_id, pattern } => {
            assert_eq!(req_id, 8);
            assert_eq!(pattern, "AAPL");
        }
        _ => panic!("expected FetchMatchingSymbols"),
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Positions
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_positions_delivers_via_wrapper() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.set_account_download_complete();
    shared.portfolio.set_position_info(PositionInfo { con_id: 265598, position_fixed: (100) as i64 * crate::types::QTY_SCALE, avg_cost: 150 * PRICE_SCALE, ..Default::default() });
    shared.portfolio.set_position_info(PositionInfo { con_id: 756733, position_fixed: (-50) as i64 * crate::types::QTY_SCALE, avg_cost: 400 * PRICE_SCALE, ..Default::default() });
    let mut w = RecordingWrapper::default();
    client.req_positions(&mut w);
    let positions: Vec<_> = w.events.iter().filter(|e| e.starts_with("position:")).collect();
    assert_eq!(positions.len(), 2);
    assert!(w.events.last().unwrap() == "position_end");
}

#[test]
fn req_positions_empty_still_calls_position_end() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.set_account_download_complete();
    let mut w = RecordingWrapper::default();
    client.req_positions(&mut w);
    assert_eq!(w.events, vec!["position_end"]);
}

// ═══════════════════════════════════════════════════════════════════
//  Scanner
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_scanner_parameters_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_scanner_parameters().unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::FetchScannerParams));
}

#[test]
fn req_scanner_subscription_sends_subscribe() {
    let (client, rx, _shared) = test_client();
    client.req_scanner_subscription(3, "STK", "STK.US.MAJOR", "TOP_PERC_GAIN", 25).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::SubscribeScanner { req_id, scan_code, max_items, .. } => {
            assert_eq!(req_id, 3);
            assert_eq!(scan_code, "TOP_PERC_GAIN");
            assert_eq!(max_items, 25);
        }
        _ => panic!("expected SubscribeScanner"),
    }
}

#[test]
fn cancel_scanner_subscription_sends_cancel() {
    let (client, rx, _shared) = test_client();
    client.cancel_scanner_subscription(3).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::CancelScanner { req_id: 3 }));
}

// ═══════════════════════════════════════════════════════════════════
//  News
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_historical_news_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_historical_news(4, 265598, "BRFG", "2026-01-01", "2026-03-01", 10).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchHistoricalNews { req_id, con_id, provider_codes, max_results, .. } => {
            assert_eq!(req_id, 4);
            assert_eq!(con_id, 265598);
            assert_eq!(provider_codes, "BRFG");
            assert_eq!(max_results, 10);
        }
        _ => panic!("expected FetchHistoricalNews"),
    }
}

#[test]
fn req_news_article_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_news_article(5, "BRFG", "BRFG$12345").unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchNewsArticle { req_id, provider_code, article_id } => {
            assert_eq!(req_id, 5);
            assert_eq!(provider_code, "BRFG");
            assert_eq!(article_id, "BRFG$12345");
        }
        _ => panic!("expected FetchNewsArticle"),
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Fundamental data
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_fundamental_data_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_fundamental_data(6, &spy(), "ReportSnapshot").unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchFundamentalData { req_id, report_type, .. } => {
            assert_eq!(req_id, 6);
            assert_eq!(report_type, "ReportSnapshot");
        }
        _ => panic!("expected FetchFundamentalData"),
    }
}

#[test]
fn cancel_fundamental_data_sends_cancel() {
    let (client, rx, _shared) = test_client();
    client.cancel_fundamental_data(6).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::CancelFundamentalData { req_id: 6 }));
}

// ═══════════════════════════════════════════════════════════════════
//  Histogram
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_histogram_data_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_histogram_data(7, &spy(), true, "1 week").unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchHistogramData { req_id, use_rth, period, .. } => {
            assert_eq!(req_id, 7);
            assert!(use_rth);
            assert_eq!(period, "1 week");
        }
        _ => panic!("expected FetchHistogramData"),
    }
}

#[test]
fn cancel_histogram_data_sends_cancel() {
    let (client, rx, _shared) = test_client();
    client.cancel_histogram_data(7).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::CancelHistogramData { req_id: 7 }));
}

// ═══════════════════════════════════════════════════════════════════
//  Historical ticks
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_historical_ticks_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_historical_ticks(8, &spy(), "20260101 09:30:00", "", 1000, "TRADES", true).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchHistoricalTicks { req_id, con_id, number_of_ticks, what_to_show, .. } => {
            assert_eq!(req_id, 8);
            assert_eq!(con_id, 756733);
            assert_eq!(number_of_ticks, 1000);
            assert_eq!(what_to_show, "TRADES");
        }
        _ => panic!("expected FetchHistoricalTicks"),
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Real-time bars
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_real_time_bars_sends_subscribe() {
    let (client, rx, _shared) = test_client();
    client.req_real_time_bars(9, &spy(), 5, "TRADES", true).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::SubscribeRealTimeBar { req_id, con_id, what_to_show, use_rth, .. } => {
            assert_eq!(req_id, 9);
            assert_eq!(con_id, 756733);
            assert_eq!(what_to_show, "TRADES");
            assert!(use_rth);
        }
        _ => panic!("expected SubscribeRealTimeBar"),
    }
}

#[test]
fn cancel_real_time_bars_sends_cancel() {
    let (client, rx, _shared) = test_client();
    client.cancel_real_time_bars(9).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::CancelRealTimeBar { req_id: 9 }));
}

// ═══════════════════════════════════════════════════════════════════
//  Historical schedule
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_historical_schedule_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_historical_schedule(11, &spy(), "20260101 16:00:00", "1 D", true).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchHistoricalSchedule { req_id, con_id, use_rth, .. } => {
            assert_eq!(req_id, 11);
            assert_eq!(con_id, 756733);
            assert!(use_rth);
        }
        _ => panic!("expected FetchHistoricalSchedule"),
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Quote / Account accessors
// ═══════════════════════════════════════════════════════════════════

#[test]
fn quote_escape_hatch() {
    let shared = Arc::new(SharedState::new());
    let mut q = Quote::default();
    q.bid = 200 * PRICE_SCALE;
    shared.market.push_quote(0, &q);

    let (tx, _rx) = crossbeam_channel::unbounded();
    let handle = std::thread::spawn(|| {});
    let client = EClient::from_parts(shared, tx, handle, "DU123".into());

    client.core.req_to_instrument.lock().unwrap().insert(5, 0);

    let quote = client.quote(5).unwrap();
    assert_eq!(quote.bid, 200 * PRICE_SCALE);
    assert!(client.quote(99).is_none());
}

// ibx#158: RTT is None until measured, then reflects the stored sample;
// req_ping goes out as a Ping command.
#[test]
fn rtt_none_until_measured_and_ping_sends_command() {
    let (client, rx, shared) = test_client();
    assert_eq!(client.last_rtt(), None);
    client.req_ping().unwrap();
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::Ping));

    shared.set_ccp_rtt(std::time::Duration::from_micros(1234));
    assert_eq!(client.last_rtt(), Some(std::time::Duration::from_micros(1234)));
}

#[test]
fn quote_by_instrument_direct() {
    let shared = Arc::new(SharedState::new());
    let mut q = Quote::default();
    q.ask = 300 * PRICE_SCALE;
    shared.market.push_quote(2, &q);

    let (tx, _rx) = crossbeam_channel::unbounded();
    let handle = std::thread::spawn(|| {});
    let client = EClient::from_parts(shared, tx, handle, "DU123".into());

    let quote = client.quote_by_instrument(2).expect("registered id");
    assert_eq!(quote.ask, 300 * PRICE_SCALE);

    // ibx#234: an out-of-range id is a caller error, not a panic across
    // the language boundary.
    assert!(client.quote_by_instrument(999).is_none());
}

#[test]
fn account_reads_shared_state() {
    let (_client, _rx, shared) = test_client();
    let mut a = AccountState::default();
    a.net_liquidation = 100_000 * PRICE_SCALE;
    shared.portfolio.set_account(&a);
    let (client2, _rx2, _) = {
        let (tx, rx) = crossbeam_channel::unbounded();
        let handle = std::thread::spawn(|| {});
        (EClient::from_parts(shared.clone(), tx, handle, "DU123".into()), rx, shared.clone())
    };
    assert_eq!(client2.account().net_liquidation, 100_000 * PRICE_SCALE);
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — fills, order updates, cancel rejects (existing)
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_fill() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_fill(Fill {
        cum_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_price: 0,
        instrument: 0, order_id: 42, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty_fixed: (100) as i64 * crate::types::QTY_SCALE, remaining_fixed: (0) as i64 * crate::types::QTY_SCALE,
        commission: PRICE_SCALE, timestamp_ns: 123456789,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:42:Filled")));
    assert!(w.events.iter().any(|e| e.starts_with("exec_details:-1:BOT:100")));
}

#[test]
fn process_msgs_dispatches_partial_fill() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_fill(Fill {
        cum_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_price: 0,
        instrument: 0, order_id: 42, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty_fixed: (50) as i64 * crate::types::QTY_SCALE, remaining_fixed: (50) as i64 * crate::types::QTY_SCALE,
        commission: PRICE_SCALE, timestamp_ns: 123456789,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    // The reference has no partially-filled status: the order stays working.
    assert!(w.events.iter().any(|e| e.starts_with("order_status:42:Submitted:50:50")), "{:?}", w.events);
}

// A fill keeps the order's working status: one last reported as
// PreSubmitted stays PreSubmitted (ib-agent#192 C8, pre-market fill).
#[test]
fn partial_fill_keeps_presubmitted() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(), lmt_price: 1.0, ..Default::default()
    };
    client.place_order(48, &spy(), &order).unwrap();
    shared.orders.push_order_update(OrderUpdate {
        order_id: 48, instrument: 0, status: OrderStatus::PreSubmitted,
        filled_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, remaining_qty_fixed: (100) as i64 * crate::types::QTY_SCALE, avg_fill_price: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 48, side: Side::Buy, price: PRICE_SCALE, qty_fixed: (40) as i64 * crate::types::QTY_SCALE, remaining_fixed: (60) as i64 * crate::types::QTY_SCALE,
        cum_qty_fixed: (40) as i64 * crate::types::QTY_SCALE, avg_price: PRICE_SCALE, commission: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:48:PreSubmitted:40:60")), "{:?}", w.events);
}

#[test]
fn process_msgs_dispatches_sell_fill() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_fill(Fill {
        cum_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_price: 0,
        instrument: 0, order_id: 43, side: Side::Sell,
        price: 151 * PRICE_SCALE, qty_fixed: (100) as i64 * crate::types::QTY_SCALE, remaining_fixed: (0) as i64 * crate::types::QTY_SCALE,
        commission: PRICE_SCALE, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("exec_details:-1:SLD:100")));
}

// The Rust client built the account batch but never called
// update_portfolio; only the Python client did. Same values and order:
// account values, portfolio rows, then the account end markers.
#[test]
fn process_msgs_delivers_update_portfolio() {
    #[derive(Default)]
    struct Rec { events: Vec<String> }
    impl Wrapper for Rec {
        fn update_account_value(&mut self, key: &str, _v: &str, _c: &str, _a: &str) {
            if key == "NetLiquidation" { self.events.push("account_value".into()); }
        }
        fn update_portfolio(
            &mut self, contract: &Contract, position: f64, market_price: f64,
            market_value: f64, average_cost: f64, unrealized_pnl: f64,
            realized_pnl: f64, account_name: &str,
        ) {
            assert_eq!((contract.sec_type.as_str(), contract.currency.as_str()), ("STK", "USD"));
            self.events.push(format!("portfolio:{}:{}:{}:{}:{}:{}:{}:{}:{}", contract.con_id, contract.symbol,
                position, market_price, market_value, average_cost, unrealized_pnl, realized_pnl, account_name));
        }
        fn account_download_end(&mut self, _a: &str) { self.events.push("download_end".into()); }
    }

    let (client, _rx, shared) = test_client();
    client.req_account_updates(true, "");
    shared.portfolio.set_position_info(crate::types::PositionInfo {
        con_id: 756733, position_fixed: (18) as i64 * crate::types::QTY_SCALE, avg_cost: 723 * PRICE_SCALE, symbol: "SPY".into(),
        sec_type: "STK".into(), currency: "USD".into(),
        ..Default::default()
    });
    shared.portfolio.set_position_marks(756733, 751 * PRICE_SCALE, 13518 * PRICE_SCALE, 504 * PRICE_SCALE, 0);
    // The account image, complete (ibx#475).
    shared.portfolio.update_account_rows(|s| {
        s.set("NetLiquidation", "USD", "1");
        s.image_complete = true;
    });

    let mut w = Rec::default();
    client.process_msgs(&mut w);
    // No cached contract: the symbol comes from the portfolio row.
    let portfolio = format!("portfolio:756733:SPY:18:751:13518:723:504:0:{}", client.account_id);
    let at = |e: &str| w.events.iter().position(|x| x == e);
    assert!(at(&portfolio).is_some(), "{:?}", w.events);
    assert!(at("account_value") < at(&portfolio) && at(&portfolio) < at("download_end"), "{:?}", w.events);

    // Unchanged on the next pass: not delivered again.
    let mut w = Rec::default();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("portfolio:")), "{:?}", w.events);
}

// ibx#313: quantities are fixed-point inside the engine; the callbacks
// report decimal shares, so half a share reaches the caller as 0.5.
#[test]
fn process_msgs_reports_a_fractional_fill_in_shares() {
    let (client, _rx, shared) = test_client();
    let q = crate::types::QTY_SCALE;
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 49, side: Side::Buy,
        price: 15 * PRICE_SCALE, qty_fixed: q / 2, remaining_fixed: q, cum_qty_fixed: q / 2,
        avg_price: 15 * PRICE_SCALE, commission: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "order_status:49:Submitted:0.5:1:15"), "{:?}", w.events);
    assert!(w.events.iter().any(|e| e == "exec_details:-1:BOT:0.5"), "{:?}", w.events);
}

// ibx#250: the reference delivers a server reject's error 201 before the
// Inactive status (ib-agent#192 C1).
#[test]
fn process_msgs_delivers_a_reject_error_before_the_status() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_order_error(47, 201, "Order rejected - reason:too big".into());
    shared.orders.push_order_update(OrderUpdate {
        order_id: 47, instrument: 0, status: OrderStatus::Rejected,
        filled_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, remaining_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_fill_price: 0,
        perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let error = w.events.iter().position(|e| e == "error:47:201:Order rejected - reason:too big");
    let status = w.events.iter().position(|e| e.starts_with("order_status:47:Inactive"));
    assert!(error.is_some() && status.is_some(), "{:?}", w.events);
    assert!(error < status, "error first: {:?}", w.events);
}

// ibx#315: filled and avgFillPrice are the order totals the fill report
// carries, not the size and price of the last print.
#[test]
fn process_msgs_reports_order_totals_on_a_multi_print_fill() {
    let (client, _rx, shared) = test_client();
    // Second print of a 300-share order: 100 @ 12 after 100 @ 10.
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 46, side: Side::Buy,
        price: 12 * PRICE_SCALE, qty_fixed: (100) as i64 * crate::types::QTY_SCALE, remaining_fixed: (100) as i64 * crate::types::QTY_SCALE,
        cum_qty_fixed: (200) as i64 * crate::types::QTY_SCALE, avg_price: 11 * PRICE_SCALE,
        commission: PRICE_SCALE, timestamp_ns: 0,
    });
    // A status report after a partial fill carries the average too.
    shared.orders.push_order_update(OrderUpdate {
        order_id: 46, instrument: 0, status: OrderStatus::Cancelled,
        filled_qty_fixed: (200) as i64 * crate::types::QTY_SCALE, remaining_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_fill_price: 11 * PRICE_SCALE,
        perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "order_status:46:Submitted:200:100:11"), "{:?}", w.events);
    assert!(w.events.iter().any(|e| e == "order_status:46:Cancelled:200:0:11"), "{:?}", w.events);
}

#[test]
fn process_msgs_dispatches_order_updates() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_order_update(OrderUpdate {
        avg_fill_price: 0,
        order_id: 43, instrument: 0, status: OrderStatus::Submitted,
        filled_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, remaining_qty_fixed: (100) as i64 * crate::types::QTY_SCALE, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    shared.orders.push_order_update(OrderUpdate {
        avg_fill_price: 0,
        order_id: 44, instrument: 0, status: OrderStatus::Cancelled,
        filled_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, remaining_qty_fixed: (100) as i64 * crate::types::QTY_SCALE, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    shared.orders.push_order_update(OrderUpdate {
        avg_fill_price: 0,
        order_id: 45, instrument: 0, status: OrderStatus::Rejected,
        filled_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, remaining_qty_fixed: (100) as i64 * crate::types::QTY_SCALE, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:43:Submitted")));
    assert!(w.events.iter().any(|e| e.starts_with("order_status:44:Cancelled")));
    assert!(w.events.iter().any(|e| e.starts_with("order_status:45:Inactive")));
}

#[test]
fn process_msgs_dispatches_cancel_reject_type_1() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_cancel_reject(CancelReject {
        order_id: 44, instrument: 0, reject_type: 1, reason_code: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:44:202:")));
}

#[test]
fn process_msgs_dispatches_cancel_reject_type_2() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_cancel_reject(CancelReject {
        order_id: 44, instrument: 0, reject_type: 2, reason_code: 5, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:44:10147:")));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — quote polling
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_quotes_on_change() {
    let (client, _rx, shared) = test_client();
    let mut q = Quote::default();
    q.bid = 150 * PRICE_SCALE;
    q.ask = 151 * PRICE_SCALE;
    shared.market.push_quote(0, &q);

    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:150")));
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:2:151")));

    // Second call — no changes, no events
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "no events on unchanged quotes");

    // Now change bid
    q.bid = 149 * PRICE_SCALE;
    shared.market.push_quote(0, &q);
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:149")));
}

#[test]
fn process_msgs_dispatches_all_quote_fields() {
    let (client, _rx, shared) = test_client();
    let q = Quote {
        bid: 150 * PRICE_SCALE, ask: 151 * PRICE_SCALE, last: 150_50000000,
        bid_size: 1000 * QTY_SCALE as i64, ask_size: 2000 * QTY_SCALE as i64,
        last_size: 500 * QTY_SCALE as i64,
        high: 155 * PRICE_SCALE, low: 148 * PRICE_SCALE,
        volume: 10_000 * QTY_SCALE as i64,
        close: 149 * PRICE_SCALE, open: 150 * PRICE_SCALE,
        timestamp_ns: 1234567890,
        bid_exch_mask: 0, ask_exch_mask: 0, last_exch_mask: 0,
    };
    shared.market.push_quote(0, &q);

    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    // Should have tick_price for: bid(1), ask(2), last(4), high(6), low(7), close(9), open(14)
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:")));   // bid
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:2:")));   // ask
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:4:")));   // last
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:6:")));   // high
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:7:")));   // low
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:9:")));   // close
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:14:"))); // open
    // tick_size for: bid_size(0), ask_size(3), last_size(5), volume(8)
    assert!(w.events.iter().any(|e| e.starts_with("tick_size:1:0:")));   // bid_size
    assert!(w.events.iter().any(|e| e.starts_with("tick_size:1:3:")));   // ask_size
    assert!(w.events.iter().any(|e| e.starts_with("tick_size:1:5:")));   // last_size
    assert!(w.events.iter().any(|e| e.starts_with("tick_size:1:8:")));   // volume
}

#[test]
fn process_msgs_multiple_instruments_independent() {
    let (client, _rx, shared) = test_client();
    let mut q0 = Quote::default();
    q0.bid = 150 * PRICE_SCALE;
    shared.market.push_quote(0, &q0);
    let mut q1 = Quote::default();
    q1.bid = 400 * PRICE_SCALE;
    shared.market.push_quote(1, &q1);

    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    client.core.req_to_instrument.lock().unwrap().insert(2, 1);
    client.core.instrument_to_req.lock().unwrap().insert(1, 2);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:150")));
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:2:1:400")));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — TBT trades / quotes
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_tbt_trade() {
    let (client, _rx, shared) = test_client();
    client.core.instrument_to_req.lock().unwrap().insert(0, 10);
    shared.market.push_tbt_trade(TbtTrade {
        instrument: 0, price: 150 * PRICE_SCALE, size: 100,
        timestamp: 1700000000, exchange: "ARCA".into(), conditions: "".into(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tbt_last:10:1:1700000000:150:100:ARCA")));
}

#[test]
fn process_msgs_dispatches_tbt_quote() {
    let (client, _rx, shared) = test_client();
    client.core.instrument_to_req.lock().unwrap().insert(0, 10);
    shared.market.push_tbt_quote(TbtQuote {
        instrument: 0, bid: 150 * PRICE_SCALE, ask: 151 * PRICE_SCALE,
        bid_size: 1000, ask_size: 2000, timestamp: 1700000000,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tbt_bidask:10:1700000000:150:151:1000:2000")));
}

#[test]
fn process_msgs_tbt_unknown_instrument_uses_neg1() {
    let (client, _rx, shared) = test_client();
    // No mapping for instrument 5
    shared.market.push_tbt_trade(TbtTrade {
        instrument: 5, price: 150 * PRICE_SCALE, size: 100,
        timestamp: 0, exchange: "".into(), conditions: "".into(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tbt_last:-1:")));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — tick news
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_tick_news() {
    let (client, _rx, shared) = test_client();
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    shared.market.push_tick_news(TickNews {
        instrument: 0,
        provider_code: "BRFG".into(), article_id: "BRFG$123".into(),
        headline: "AAPL beats".into(), timestamp: 1700000000,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "tick_news:BRFG:BRFG$123:AAPL beats"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — news bulletins
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_news_bulletin() {
    let (client, _rx, shared) = test_client();
    client.req_news_bulletins(true);
    shared.market.push_news_bulletin(NewsBulletin {
        msg_id: 1, msg_type: 1,
        message: "Exchange notice".into(), exchange: "NYSE".into(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "news_bulletin:1:1:Exchange notice:NYSE"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — what-if
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_what_if() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_what_if(WhatIfResponse {
        order_id: 42, instrument: 0,
        init_margin_before: 0, maint_margin_before: 0,
        equity_with_loan_before: 0,
        init_margin_after: 5000 * PRICE_SCALE,
        maint_margin_after: 3000 * PRICE_SCALE,
        equity_with_loan_after: 0,
        commission: PRICE_SCALE,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:42:PreSubmitted")));
}

/// Regression: what-if dispatch must populate all 8 OrderState fields and call
/// open_order BEFORE order_status, matching official ibapi contract.
#[test]
fn process_msgs_what_if_emits_full_order_state() {
    let (client, _rx, shared) = test_client();
    // Distinct values per field so any swap/typo is detectable.
    shared.orders.push_what_if(WhatIfResponse {
        order_id: 7, instrument: 0,
        init_margin_before:    100 * PRICE_SCALE,
        maint_margin_before:   200 * PRICE_SCALE,
        equity_with_loan_before: 300 * PRICE_SCALE,
        init_margin_after:     400 * PRICE_SCALE,
        maint_margin_after:    500 * PRICE_SCALE,
        equity_with_loan_after: 600 * PRICE_SCALE,
        commission:            7 * PRICE_SCALE,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    let open_idx = w.events.iter().position(|e| e.starts_with("open_order:7:"))
        .expect("open_order callback missing for what-if");
    let status_idx = w.events.iter().position(|e| e.starts_with("order_status:7:PreSubmitted"))
        .expect("order_status callback missing for what-if");
    assert!(open_idx < status_idx, "open_order must be emitted before order_status");

    let evt = &w.events[open_idx];
    // status, all 9 margin fields (before/change/after × init/maint/eql), commission.
    assert!(evt.contains(":PreSubmitted:"), "status field missing: {evt}");
    assert!(evt.contains("initB=100.00:initC=300.00:initA=400.00"), "init margin wrong: {evt}");
    assert!(evt.contains("maintB=200.00:maintC=300.00:maintA=500.00"), "maint margin wrong: {evt}");
    assert!(evt.contains("eqlB=300.00:eqlC=300.00:eqlA=600.00"), "equity-with-loan wrong: {evt}");
    assert!(evt.contains("comm=7"), "commission wrong: {evt}");
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — historical data
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_historical_data() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_data(5, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![
            HistoricalBar { time: "20260101".into(), open: 100.0, high: 105.0, low: 99.0, close: 103.0, volume: 1000, wap: 102.0, count: 50 },
            HistoricalBar { time: "20260102".into(), open: 103.0, high: 108.0, low: 102.0, close: 107.0, volume: 1200, wap: 105.0, count: 60 },
        ],
        is_complete: true,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "historical_data:5:20260101"));
    assert!(w.events.iter().any(|e| e == "historical_data:5:20260102"));
    assert!(w.events.iter().any(|e| e == "historical_data_end:5"));
}

#[test]
fn process_msgs_historical_data_incomplete_no_end() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_data(5, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![
            HistoricalBar { time: "20260101".into(), open: 100.0, high: 105.0, low: 99.0, close: 103.0, volume: 1000, wap: 102.0, count: 50 },
        ],
        is_complete: false,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "historical_data:5:20260101"));
    assert!(!w.events.iter().any(|e| e == "historical_data_end:5"), "no end for incomplete");
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — head timestamps
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_head_timestamp() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_head_timestamp(10, HeadTimestampResponse { head_timestamp: "20200101".into(), timezone: String::new() });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "head_timestamp:10:20200101"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — contract details
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_contract_details() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_contract_details(7, ContractDefinition {
        con_id: 265598, symbol: "AAPL".into(), sec_type: SecurityType::Stock,
        exchange: "SMART".into(), primary_exchange: "NASDAQ".into(),
        currency: "USD".into(), local_symbol: "AAPL".into(),
        trading_class: "AAPL".into(), long_name: "Apple Inc".into(),
        min_tick: 0.01, multiplier: 1.0, valid_exchanges: vec!["SMART".into()],
        order_types: vec!["LMT".into()], market_rule_id: Some(26),
        last_trade_date: String::new(), right: None, strike: 0.0,
        ..Default::default()
    });
    shared.reference.push_contract_details_end(7);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "contract_details:7:AAPL"));
    assert!(w.events.iter().any(|e| e == "contract_details_end:7"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — matching symbols
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_symbol_samples() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_matching_symbols(8, vec![
        SymbolMatch {
            con_id: 265598, symbol: "AAPL".into(), sec_type: SecurityType::Stock,
            currency: "USD".into(), primary_exchange: "NASDAQ".into(),
            description: "Apple Inc".into(), derivative_types: vec!["OPT".into()],
        },
    ]);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "symbol_samples:8:1"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — scanner
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_scanner_params() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_scanner_params("<scanner>XML</scanner>".into());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "scanner_parameters"));
}

#[test]
fn process_msgs_dispatches_scanner_data() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_scanner_data(3, ScannerResult {
        con_ids: vec![265598, 756733],
        entries: vec![
            ScannerEntry { con_id: 265598, ..Default::default() },
            ScannerEntry { con_id: 756733, ..Default::default() },
        ],
        scan_time: "2026-03-13".into(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "scanner_data:3:0"));
    assert!(w.events.iter().any(|e| e == "scanner_data:3:1"));
    assert!(w.events.iter().any(|e| e == "scanner_data_end:3"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — news
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_historical_news() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_news(4, vec![
        NewsHeadline {
            time: "2026-01-15".into(), provider_code: "BRFG".into(),
            article_id: "BRFG$100".into(), headline: "Earnings beat".into(),
        },
        NewsHeadline {
            time: "2026-01-16".into(), provider_code: "BRFG".into(),
            article_id: "BRFG$101".into(), headline: "Guidance raised".into(),
        },
    ], false);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "historical_news:4:BRFG:BRFG$100:Earnings beat"));
    assert!(w.events.iter().any(|e| e == "historical_news:4:BRFG:BRFG$101:Guidance raised"));
    assert!(w.events.iter().any(|e| e == "historical_news_end:4:false"));
}

#[test]
fn process_msgs_dispatches_news_article() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_news_article(5, 0, "Full article text here".into());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "news_article:5:0:Full article text here"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — fundamental data
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_fundamental_data() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_fundamental_data(6, "<report>data</report>".into());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "fundamental_data:6"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — histogram data
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_histogram_data() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_histogram_data(7, vec![
        HistogramEntry { price: 150.0, count: 500 },
        HistogramEntry { price: 151.0, count: 300 },
    ]);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "histogram_data:7:2"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — historical ticks
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_historical_ticks() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_ticks(8, HistoricalTickData::Midpoint(vec![
        HistoricalTickMidpoint { time: "2026-01-15 09:30:00".into(), price: 150.5 },
    ]), "MIDPOINT".into(), true);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "historical_ticks:8:true"));
}

/// Regression: historical-tick variants must route to their variant-specific
/// callback (iso ibapi). Was: all three flowed through historical_ticks().
#[test]
fn process_msgs_routes_historical_tick_variants() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_ticks(10, HistoricalTickData::Last(vec![
        HistoricalTickLast {
            time: "2026-01-15 09:30:00".into(), price: 150.5, size: 100,
            exchange: "ARCA".into(), special_conditions: "".into(),
        },
    ]), "TRADES".into(), true);
    shared.reference.push_historical_ticks(11, HistoricalTickData::BidAsk(vec![
        HistoricalTickBidAsk {
            time: "2026-01-15 09:30:01".into(), bid_price: 150.4, ask_price: 150.6,
            bid_size: 200, ask_size: 300,
        },
    ]), "BID_ASK".into(), true);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    assert!(w.events.iter().any(|e| e == "historical_ticks_last:10:true"),
        "Last variant must route to historical_ticks_last; got {:?}", w.events);
    assert!(w.events.iter().any(|e| e == "historical_ticks_bid_ask:11:true"),
        "BidAsk variant must route to historical_ticks_bid_ask; got {:?}", w.events);
    // Generic historical_ticks should NOT fire for Last or BidAsk.
    assert!(!w.events.iter().any(|e| e == "historical_ticks:10:true"));
    assert!(!w.events.iter().any(|e| e == "historical_ticks:11:true"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — real-time bars
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_real_time_bar() {
    let (client, _rx, shared) = test_client();
    shared.market.push_real_time_bar(9, RealTimeBar {
        timestamp: 1700000000, open: 150.0, high: 151.0,
        low: 149.0, close: 150.5, volume: 1000.0, wap: 150.25, count: 50,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("real_time_bar:9:1700000000")));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — historical schedule
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_historical_schedule() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_schedule(11, HistoricalScheduleResponse {
        query_id: String::new(),
        timezone: "US/Eastern".into(),
        start_date_time: "20260101".into(),
        end_date_time: "20260102".into(),
        sessions: vec![ScheduleSession {
            ref_date: "20260101".into(),
            open_time: "09:30:00".into(),
            close_time: "16:00:00".into(),
        }],
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "historical_schedule:11:US/Eastern:1"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — drain is exhaustive
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_empty_queues_no_events() {
    let (client, _rx, _shared) = test_client();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty());
}

#[test]
fn process_msgs_drains_on_first_call_empty_on_second() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_fill(Fill {
        cum_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_price: 0,
        instrument: 0, order_id: 1, side: Side::Buy,
        price: PRICE_SCALE, qty_fixed: (1) as i64 * crate::types::QTY_SCALE, remaining_fixed: (0) as i64 * crate::types::QTY_SCALE,
        commission: 0, timestamp_ns: 0,
    });
    shared.orders.push_order_update(OrderUpdate {
        avg_fill_price: 0,
        order_id: 2, instrument: 0, status: OrderStatus::Submitted,
        filled_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, remaining_qty_fixed: (1) as i64 * crate::types::QTY_SCALE, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(!w.events.is_empty());

    w.events.clear();
    client.process_msgs(&mut w);
    // Only quote events might fire (if mapped), but no fills/updates
    let non_tick_events: Vec<_> = w.events.iter()
        .filter(|e| !e.starts_with("tick_price") && !e.starts_with("tick_size"))
        .collect();
    assert!(non_tick_events.is_empty(), "second drain should be empty");
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — a live exec_details has reqId -1 (ibx#474)
// ═══════════════════════════════════════════════════════════════════

// The reference sends a live execution with reqId -1. ibx used the market
// data reqId of the instrument.
#[test]
fn process_msgs_live_fill_has_req_id_minus_one() {
    let (client, _rx, shared) = test_client();
    client.core.instrument_to_req.lock().unwrap().insert(0, 42);
    shared.orders.push_fill(Fill {
        cum_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_price: 0,
        instrument: 0, order_id: 1, side: Side::Buy,
        price: PRICE_SCALE, qty_fixed: (100) as i64 * crate::types::QTY_SCALE, remaining_fixed: (0) as i64 * crate::types::QTY_SCALE,
        commission: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("exec_details:-1:")), "{:?}", w.events);
}

// ── Order modification edge cases ─────────────────────────────────

#[test]
fn modify_limit_order_price_via_resubmit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0, ..Default::default()
    };
    client.place_order(80, &spy(), &order).unwrap();
    while rx.try_recv().is_ok() {}

    let modified = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 152.0, ..Default::default()
    };
    client.place_order(80, &spy(), &modified).unwrap();

    let mut found = false;
    while let Ok(cmd) = rx.try_recv() {
        if let ControlCommand::Order(OrderRequest::Modify { order_id: 80, kind, qty, .. }) = cmd {
            assert!(matches!(kind, OrderKind::Limit { price } if price == (152.0 * PRICE_SCALE_F) as i64));
            assert_eq!(qty, 100);
            found = true;
        }
    }
    assert!(found, "Resubmit with same orderId should emit Modify");
}

#[test]
fn modify_order_before_ack_no_panic() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    for price in 0..10 {
        let order = Order {
            action: "BUY".into(), total_quantity: 100.0,
            order_type: "LMT".into(), lmt_price: 150.0 + price as f64,
            ..Default::default()
        };
        let _ = client.place_order(42, &spy(), &order);
    }
    let mut count = 0;
    while rx.try_recv().is_ok() { count += 1; }
    assert!(count >= 10, "All modify attempts should send commands, got {}", count);
}

#[test]
fn cancel_during_modify_no_panic() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0, ..Default::default()
    };
    client.place_order(99, &spy(), &order).unwrap();
    let modified = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 151.0, ..Default::default()
    };
    client.place_order(99, &spy(), &modified).unwrap();
    client.cancel_order(99, "").unwrap();

    let mut has_cancel = false;
    while let Ok(cmd) = rx.try_recv() {
        if matches!(cmd, ControlCommand::Order(OrderRequest::Cancel { order_id: 99 })) {
            has_cancel = true;
        }
    }
    assert!(has_cancel, "Cancel command should be sent");
}

#[test]
fn modify_filled_order_receives_cancel_reject() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);
    shared.orders.push_fill(Fill {
        cum_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_price: 0,
        instrument: 0, order_id: 120, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty_fixed: (100) as i64 * crate::types::QTY_SCALE, remaining_fixed: (0) as i64 * crate::types::QTY_SCALE,
        commission: 0, timestamp_ns: 1000,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:120:Filled")));

    shared.orders.push_cancel_reject(CancelReject {
        order_id: 120, instrument: 0, reject_type: 2, reason_code: 0, timestamp_ns: 2000,
    });
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:120:")),
        "Modify reject should generate error callback, got: {:?}", w.events);
}

#[test]
fn rapid_modify_multiple_prices_no_crash() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    for i in 0..50 {
        let order = Order {
            action: "BUY".into(), total_quantity: 100.0,
            order_type: "LMT".into(), lmt_price: 100.0 + i as f64 * 0.01,
            ..Default::default()
        };
        let _ = client.place_order(77, &spy(), &order);
    }
    let mut order_count = 0;
    while let Ok(cmd) = rx.try_recv() {
        if matches!(cmd, ControlCommand::Order(_)) { order_count += 1; }
    }
    assert_eq!(order_count, 50, "All 50 modify commands should be sent");
}

#[test]
fn modify_tif_day_to_gtc_via_resubmit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0,
        tif: "DAY".into(), ..Default::default()
    };
    client.place_order(88, &spy(), &order).unwrap();
    while rx.try_recv().is_ok() {}

    let modified = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0,
        tif: "GTC".into(), ..Default::default()
    };
    client.place_order(88, &spy(), &modified).unwrap();

    let mut found_modify = false;
    while let Ok(cmd) = rx.try_recv() {
        if let ControlCommand::Order(OrderRequest::Modify { order_id: 88, kind, qty, tif, .. }) = cmd {
            assert!(matches!(kind, OrderKind::Limit { price } if price == (150.0 * PRICE_SCALE_F) as i64));
            assert_eq!(qty, 100);
            // ibx#349: the new time-in-force must reach the replace.
            assert_eq!(tif, b'1', "DAY -> GTC must be carried");
            found_modify = true;
        }
    }
    assert!(found_modify, "Resubmit with same orderId should emit Modify");
}

#[test]
fn modify_price_and_qty_simultaneously() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0, ..Default::default()
    };
    client.place_order(55, &spy(), &order).unwrap();
    while rx.try_recv().is_ok() {}

    let modified = Order {
        action: "BUY".into(), total_quantity: 200.0,
        order_type: "LMT".into(), lmt_price: 148.0, ..Default::default()
    };
    client.place_order(55, &spy(), &modified).unwrap();

    let mut found = false;
    while let Ok(cmd) = rx.try_recv() {
        if let ControlCommand::Order(OrderRequest::Modify { order_id: 55, qty, kind, .. }) = cmd {
            assert_eq!(qty, 200);
            assert!(matches!(kind, OrderKind::Limit { price } if price == (148.0 * PRICE_SCALE_F) as i64));
            found = true;
        }
    }
    assert!(found, "Resubmit with same orderId should emit Modify with new price and qty");
}

#[test]
fn modify_order_type_lmt_to_stp() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0, ..Default::default()
    };
    client.place_order(66, &spy(), &order).unwrap();
    while rx.try_recv().is_ok() {}

    let modified = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "STP".into(), aux_price: 149.0, ..Default::default()
    };
    client.place_order(66, &spy(), &modified).unwrap();

    // The reference refuses a change of order type before sending anything:
    // error 329, no replace, the order stays LMT (ib-agent#192 A4b, ibx#349).
    assert!(rx.try_recv().is_err(), "no replace may be sent for a type change");
    let errors = shared.orders.drain_order_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 66);
    assert_eq!(errors[0].1, 329);
    assert!(errors[0].2.ends_with("Cannot change to the new order type.STP"), "{}", errors[0].2);
    assert_eq!(client.core.tracked_order_type(66).as_deref(), Some("LMT"));
}

// The refusal must carry the full order id: ibx ids do not fit in 32 bits.
#[test]
fn modify_type_change_error_keeps_a_large_order_id() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let id: i64 = 1_790_166_425_204;
    let lmt = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 1.0, ..Default::default() };
    let stp = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "STP".into(), aux_price: 2.0, ..Default::default() };
    client.place_order(id, &spy(), &lmt).unwrap();
    while rx.try_recv().is_ok() {}
    client.place_order(id, &spy(), &stp).unwrap();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with(&format!("error:{}:329:", id))), "{:?}", w.events);
}

/// Place `first`, then resubmit `second` with the same id; return the replace.
fn modify_of(first: Order, second: Order) -> (u32, OrderKind, u8, OrderAttrs) {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    client.place_order(90, &spy(), &first).unwrap();
    while rx.try_recv().is_ok() {}
    client.place_order(90, &spy(), &second).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::Modify { order_id: 90, qty, kind, tif, attrs, .. }) => (qty, kind, tif, attrs),
        other => panic!("expected Modify, got {:?}", other),
    }
}

// ibx#324: a stop modify must carry the new trigger, not a limit price of 0.
#[test]
fn modify_stop_moves_the_trigger() {
    let stp = |aux: f64| Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(), aux_price: aux, ..Default::default()
    };
    let (_, kind, _, _) = modify_of(stp(100.0), stp(95.0));
    assert!(matches!(kind, OrderKind::Stop { stop_price } if stop_price == (95.0 * PRICE_SCALE_F) as i64), "{:?}", kind);
}

// ibx#324: a stop-limit modify moves both prices.
#[test]
fn modify_stop_limit_moves_both_prices() {
    let stp_lmt = |lmt: f64, aux: f64| Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "STP LMT".into(),
        lmt_price: lmt, aux_price: aux, ..Default::default()
    };
    let (_, kind, _, _) = modify_of(stp_lmt(99.0, 100.0), stp_lmt(94.0, 95.0));
    assert!(matches!(kind, OrderKind::StopLimit { price, stop_price }
        if price == (94.0 * PRICE_SCALE_F) as i64 && stop_price == (95.0 * PRICE_SCALE_F) as i64), "{:?}", kind);
}

// ibx#334: a trailing modify keeps its trailing kind and amount / percent.
#[test]
fn modify_trailing_keeps_the_trail() {
    let trail = |aux: f64| Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL".into(), aux_price: aux, ..Default::default()
    };
    let (_, kind, _, _) = modify_of(trail(2.0), trail(3.0));
    assert!(matches!(kind, OrderKind::TrailingStop { trail_amt, .. } if trail_amt == (3.0 * PRICE_SCALE_F) as i64), "{:?}", kind);

    let pct = |p: f64| Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL".into(), trailing_percent: p, ..Default::default()
    };
    let (_, kind, _, _) = modify_of(pct(1.0), pct(2.5));
    assert!(matches!(kind, OrderKind::TrailPct { trail_pct: 250, .. }), "{:?}", kind);
}

// ibx#339: a percent is rounded to basis points, not truncated: 1.15 %
// became 114 bp (1.14 %) through float truncation.
#[test]
fn percent_trail_rounds_to_basis_points() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL".into(),
        trailing_percent: 1.15, ..Default::default()
    };
    client.place_order(91, &spy(), &order).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitTrailingStopPct { trail_pct, .. }) => assert_eq!(trail_pct, 115),
        other => panic!("expected SubmitTrailingStopPct, got {:?}", other),
    }
    assert!(matches!(ClientCore::order_kind(&order).unwrap(), OrderKind::TrailPct { trail_pct: 115, .. }));
}

// ibx#313: a fractional quantity was cut to a whole number and sent (1.5
// shares went out as 1). The reference refuses it before sending, with
// error 10243 (ib-agent#192 B3).
#[test]
fn fractional_quantity_is_refused_before_sending() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.5, order_type: "LMT".into(), lmt_price: 1.0, ..Default::default()
    };
    client.place_order(93, &spy(), &order).unwrap();
    assert!(rx.try_recv().is_err(), "nothing may be sent");
    assert!(!client.core.is_order_tracked(93));
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:93:10243:Fractional-sized order")), "{:?}", w.events);
}

// ibx#263: bad algo parameter values were turned into defaults and sent.
// The reference refuses each captured case before sending, with these
// codes and texts (ib-agent#192 B10a-e).
#[test]
fn bad_algo_parameter_values_are_refused_before_sending() {
    let cases: [(&str, &str, &str, &str); 5] = [
        ("Adaptive", "adaptivePriority", "Bogus", "145:Error in validating entry fields -Bogus"),
        ("ArrivalPx", "riskAversion", "Bogus", "145:Error in validating entry fields -Bogus"),
        ("Vwap", "maxPctVol", "NaN",
            "441:Algo attributes validation failed: 'Max Percentage' is invalid: Value is greater than maximum value 50.0.. "),
        ("Vwap", "maxPctVol", "-0.1",
            "441:Algo attributes validation failed: 'Max Percentage' is invalid: Value is less than minimum value 0.01.. "),
        ("PctVol", "pctVol", "-0.5",
            "441:Algo attributes validation failed: 'Target Percentage' is invalid: Value is less than minimum value 0.01.. "),
    ];
    for (i, (strategy, tag, value, expected)) in cases.into_iter().enumerate() {
        let (client, rx, shared) = test_client();
        shared.market.set_instrument_count(1);
        let id = 100 + i as i64;
        let order = Order {
            action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 1.0,
            algo_strategy: strategy.into(),
            algo_params: vec![TagValue { tag: tag.into(), value: value.into() }],
            ..Default::default()
        };
        client.place_order(id, &spy(), &order).unwrap();
        assert!(rx.try_recv().is_err(), "{} {}={}: nothing may be sent", strategy, tag, value);
        let mut w = RecordingWrapper::default();
        client.process_msgs(&mut w);
        let want = format!("error:{}:{}", id, expected);
        assert!(w.events.iter().any(|e| *e == want), "{} {}={}: {:?}", strategy, tag, value, w.events);
    }
}

// Values the reference accepted in the same capture still go out.
#[test]
fn valid_algo_parameter_values_are_sent() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 1.0,
        algo_strategy: "ArrivalPx".into(),
        algo_params: vec![
            TagValue { tag: "maxPctVol".into(), value: "0.1".into() },
            TagValue { tag: "riskAversion".into(), value: "Neutral".into() },
        ],
        ..Default::default()
    };
    client.place_order(110, &spy(), &order).unwrap();
    assert!(matches!(rx.try_recv(), Ok(ControlCommand::Order(OrderRequest::SubmitAlgo { .. }))));
}

#[test]
fn fractional_quantity_on_a_modify_is_refused_and_keeps_the_order() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let whole = Order {
        action: "BUY".into(), total_quantity: 2.0, order_type: "LMT".into(), lmt_price: 1.0, ..Default::default()
    };
    client.place_order(94, &spy(), &whole).unwrap();
    while rx.try_recv().is_ok() {}
    let frac = Order { total_quantity: 2.5, ..whole.clone() };
    client.place_order(94, &spy(), &frac).unwrap();
    assert!(rx.try_recv().is_err(), "no replace may be sent");
    assert!(client.core.is_order_tracked(94), "the working order stays tracked");
    let errors = shared.orders.drain_order_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!((errors[0].0, errors[0].1), (94, 10243));
}

// ibx#247: outside-RTH follows the order; it is not forced on.
#[test]
fn modify_carries_outside_rth_as_set() {
    let lmt = |rth: bool, px: f64| Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: px,
        outside_rth: rth, ..Default::default()
    };
    let (_, _, _, attrs) = modify_of(lmt(false, 10.0), lmt(false, 11.0));
    assert!(!attrs.outside_rth);
    let (_, _, _, attrs) = modify_of(lmt(true, 10.0), lmt(true, 11.0));
    assert!(attrs.outside_rth);
}

// ── Market data type switching ────────────────────────────────────

#[test]
fn market_data_type_callback_compiles_and_dispatches() {
    struct MarketDataTypeRecorder { events: Vec<(i64, i32)> }
    impl crate::api::wrapper::Wrapper for MarketDataTypeRecorder {
        fn market_data_type(&mut self, req_id: i64, market_data_type: i32) {
            self.events.push((req_id, market_data_type));
        }
    }
    let mut w = MarketDataTypeRecorder { events: vec![] };
    w.market_data_type(1, 1); // Live
    w.market_data_type(1, 2); // Frozen
    w.market_data_type(1, 3); // Delayed
    w.market_data_type(1, 4); // Delayed-Frozen
    assert_eq!(w.events.len(), 4);
    assert_eq!(w.events[0], (1, 1));
    assert_eq!(w.events[3], (1, 4));
}

#[test]
fn quote_dispatch_agnostic_to_data_type() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);
    let mut q = Quote::default();
    q.bid = 450 * PRICE_SCALE;
    q.ask = 451 * PRICE_SCALE;
    shared.market.push_quote(0, &q);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:450")));
}

#[test]
fn frozen_stale_quote_no_redispatch() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);
    let mut q = Quote::default();
    q.bid = 300 * PRICE_SCALE;
    q.ask = 301 * PRICE_SCALE;
    shared.market.push_quote(0, &q);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:")));

    shared.market.push_quote(0, &q); // same quote
    w.events.clear();
    client.process_msgs(&mut w);
    let second_count = w.events.iter().filter(|e| e.starts_with("tick_price:1:")).count();
    assert_eq!(second_count, 0, "Identical frozen quote should not re-dispatch");
}

#[test]
fn transition_no_data_to_live_fires_callbacks() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events.iter().filter(|e| e.starts_with("tick_price:1:")).count(), 0);

    let mut q = Quote::default();
    q.bid = 500 * PRICE_SCALE;
    q.ask = 501 * PRICE_SCALE;
    shared.market.push_quote(0, &q);
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:500")));
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:2:501")));
}

#[test]
fn partial_quote_update_only_changed_fields_dispatch() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);
    let mut q = Quote::default();
    q.bid = 100 * PRICE_SCALE;
    q.ask = 101 * PRICE_SCALE;
    shared.market.push_quote(0, &q);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    q.bid = 99 * PRICE_SCALE;
    shared.market.push_quote(0, &q);
    w.events.clear();
    client.process_msgs(&mut w);

    let bid_ticks: Vec<_> = w.events.iter().filter(|e| e.starts_with("tick_price:1:1:")).collect();
    let ask_ticks: Vec<_> = w.events.iter().filter(|e| e.starts_with("tick_price:1:2:")).collect();
    assert!(!bid_ticks.is_empty(), "Changed bid should dispatch");
    assert!(ask_ticks.is_empty(), "Unchanged ask should NOT dispatch");
}

// ═══════════════════════════════════════════════════════════════════
//  Thread lifecycle
// ═══════════════════════════════════════════════════════════════════

#[test]
fn disconnect_joins_thread() {
    let (client, _rx, _shared) = test_client();
    // The test_client helper spawns an empty thread (already exited).
    // disconnect() should join it without hanging.
    client.disconnect();
    assert!(!client.is_connected());
}

#[test]
fn drop_without_disconnect_joins_thread() {
    let (client, _rx, _shared) = test_client();
    // Dropping without explicit disconnect — Drop impl should join.
    drop(client);
    // No hang = success.
}

#[test]
fn disconnect_is_idempotent() {
    let (client, _rx, _shared) = test_client();
    client.disconnect();
    // Second disconnect should not panic (thread already joined).
    client.disconnect();
    assert!(!client.is_connected());
}

#[test]
fn ccp_session_id_matches_shared_reference() {
    let (client, _rx, shared) = test_client();
    assert_eq!(client.ccp_session_id(), shared.reference.ccp_session_id());

    shared.reference.set_ccp_session_id("sid.0001".to_string());
    assert_eq!(client.ccp_session_id(), "sid.0001");
    assert_eq!(client.ccp_session_id(), client.shared.reference.ccp_session_id());
}

#[test]
fn misc_url_lookup_delegates_to_shared() {
    let (client, _rx, shared) = test_client();
    assert!(client.misc_url("region_dam").is_none());

    let mut urls = std::collections::HashMap::new();
    urls.insert("region_dam".to_string(), "api.example.com".to_string());
    shared.reference.set_misc_urls(urls);

    assert_eq!(client.misc_url("region_dam").as_deref(), Some("api.example.com"));
    assert!(client.misc_url("missing").is_none());
}

#[test]
fn session_token_bytes_roundtrip_through_biguint() {
    use num_bigint::BigUint;

    let shared = Arc::new(SharedState::new());
    let (tx, _rx) = crossbeam_channel::unbounded();
    let handle = std::thread::spawn(|| {});
    let mut client = EClient::from_parts(shared, tx, handle, "DU123".into());

    let session_token = BigUint::parse_bytes(
        b"fedcba9876543210fedcba9876543210", 16,
    ).unwrap();
    client.session_token_bytes = crate::auth::crypto::strip_leading_zeros(
        &session_token.to_bytes_be(),
    ).to_vec();

    assert_eq!(BigUint::from_bytes_be(client.session_token_bytes()), session_token);
}

#[test]
fn token_type_default_is_empty() {
    let (client, _rx, _shared) = test_client();
    assert_eq!(client.token_type(), "");
}

// ═══════════════════════════════════════════════════════════════════
//  Connection loss (ibx#242)
// ═══════════════════════════════════════════════════════════════════

#[test]
fn engine_connection_loss_fires_connection_closed_once() {
    let (client, _rx, shared) = test_client();
    let mut w = RecordingWrapper::default();

    // Nothing to report while the engine is running.
    client.process_msgs(&mut w);
    assert!(client.is_connected());
    assert!(w.events.is_empty(), "no callbacks before the connection is lost");

    // Engine signals the end of the session.
    shared.set_connection_lost();
    client.process_msgs(&mut w);

    assert_eq!(w.events, vec!["connection_closed"]);
    assert!(!client.is_connected(), "is_connected must turn false");

    // Polling again must not repeat it.
    client.process_msgs(&mut w);
    assert_eq!(w.events, vec!["connection_closed"]);
}

#[test]
fn connection_loss_raises_no_error_callback() {
    // The reference client fires connection_closed with no error code on a
    // lost socket; the connectivity codes are server-pushed, never local.
    let (client, _rx, shared) = test_client();
    let mut w = RecordingWrapper::default();

    shared.set_connection_lost();
    client.process_msgs(&mut w);

    assert!(
        !w.events.iter().any(|e| e.starts_with("error:")),
        "no error callback expected, got: {:?}", w.events,
    );
}

#[test]
fn explicit_disconnect_fires_connection_closed() {
    let (client, _rx, _shared) = test_client();
    let mut w = RecordingWrapper::default();

    client.disconnect();
    client.process_msgs(&mut w);

    assert_eq!(w.events, vec!["connection_closed"]);
    assert!(!client.is_connected());
}

#[test]
fn queued_data_is_dispatched_before_connection_closed() {
    // A caller that stops polling on connection_closed must still have seen
    // whatever the engine had already queued.
    let (client, _rx, shared) = test_client();
    let mut w = RecordingWrapper::default();

    shared.reference.push_contract_details_end(7);
    shared.set_connection_lost();
    client.process_msgs(&mut w);

    assert_eq!(w.events, vec!["contract_details_end:7", "connection_closed"]);
}

// ═══════════════════════════════════════════════════════════════════
//  Commission reports from their own server frame (ibx#471)
// ═══════════════════════════════════════════════════════════════════

fn aapl_fill(order_id: u64) -> Fill {
    Fill {
        instrument: 0, order_id, side: Side::Buy, price: 336 * PRICE_SCALE,
        qty_fixed: 100 * crate::types::QTY_SCALE, remaining_fixed: 0,
        cum_qty_fixed: 100 * crate::types::QTY_SCALE, avg_price: 336 * PRICE_SCALE,
        commission: 0, timestamp_ns: 0,
    }
}

fn captured_report() -> crate::api::types::CommissionAndFeesReport {
    crate::api::types::CommissionAndFeesReport {
        exec_id: "0000e0d5.6ab5f36f.01.01".into(), commission_and_fees: 1.0003,
        currency: "USD".into(), realized_pnl: f64::MAX, yield_amount: f64::MAX,
        yield_redemption_date: String::new(),
    }
}

// The fill carries no commission: the report comes from the commission
// frame, after exec_details, with the server's values.
#[test]
fn commission_report_comes_from_the_commission_frame() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_fill_with_exec(aapl_fill(7), crate::bridge::FillExec { exec_id: "0000e0d5.6ab5f36f.01.01".into(), ..Default::default() });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("commission:")), "no report from the fill: {:?}", w.events);

    shared.orders.push_commission_report(captured_report());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["commission:0000e0d5.6ab5f36f.01.01:1.0003:USD"]);

    let mut w = RecordingWrapper::default();
    client.req_executions(1, &crate::api::types::ExecutionFilter::default(), &mut w);
    assert_eq!(w.events, [
        "exec_details:1:BOT:100",
        "commission:0000e0d5.6ab5f36f.01.01:1.0003:USD",
        "exec_details_end:1",
    ]);
}

// A report that comes before its execution waits for it, and is sent
// after exec_details.
#[test]
fn commission_report_before_its_execution_waits_for_it() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_commission_report(captured_report());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);

    shared.orders.push_fill_with_exec(aapl_fill(7), crate::bridge::FillExec { exec_id: "0000e0d5.6ab5f36f.01.01".into(), ..Default::default() });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let exec = w.events.iter().position(|e| e.starts_with("exec_details:")).expect("exec_details");
    let comm = w.events.iter().position(|e| e.starts_with("commission:")).expect("commission");
    assert!(exec < comm, "{:?}", w.events);
}

// Before the commission frame, req_executions replays the execution alone.
#[test]
fn req_executions_without_a_commission_report_sends_the_execution_only() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_fill_with_exec(aapl_fill(7), crate::bridge::FillExec { exec_id: "0000e0d5.6ab5f36f.01.01".into(), ..Default::default() });
    client.process_msgs(&mut RecordingWrapper::default());
    let mut w = RecordingWrapper::default();
    client.req_executions(1, &crate::api::types::ExecutionFilter::default(), &mut w);
    assert_eq!(w.events, ["exec_details:1:BOT:100", "exec_details_end:1"]);
}

// ═══════════════════════════════════════════════════════════════════
//  exec_details fields and the req_executions filter (ibx#474)
// ═══════════════════════════════════════════════════════════════════

/// Captured fill of 25/09/2026: BUY 100 AAPL, clientId 250, orderRef
/// pm0925-fill-BUY, time 20260925-08:49:54 UTC, routed to ARCA.
fn captured_fill_exec() -> crate::bridge::FillExec {
    crate::bridge::FillExec {
        exec_id: "0000e0d5.6ab5f36f.01.01".into(),
        time_secs: Some(1790326194),
        exchange: "ARCA".into(),
        client_id: 250,
        model_code: String::new(),
        order_ref: "pm0925-fill-BUY".into(),
    }
}

#[derive(Default)]
struct ExecRecorder { execs: Vec<(i64, crate::api::types::Execution)> }
impl Wrapper for ExecRecorder {
    fn exec_details(&mut self, req_id: i64, _c: &Contract, e: &crate::api::types::Execution) {
        self.execs.push((req_id, e.clone()));
    }
}

#[test]
fn exec_details_carries_the_fill_report_fields() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_fill_with_exec(aapl_fill(7), captured_fill_exec());
    let mut w = ExecRecorder::default();
    client.process_msgs(&mut w);
    let (req_id, e) = &w.execs[0];
    assert_eq!(*req_id, -1);
    assert_eq!(e.exec_id, "0000e0d5.6ab5f36f.01.01");
    assert_eq!(e.time, "20260925 04:49:54 US/Eastern");
    assert_eq!(e.exchange, "ARCA");
    assert_eq!(e.client_id, 250);
    assert_eq!(e.order_ref, "pm0925-fill-BUY");
    assert_eq!(e.side, "BOT");
}

// ibx sends no clientId or orderRef on its own orders: the execution takes
// this client's id and the tracked order's orderRef.
#[test]
fn exec_details_of_an_own_order_takes_the_tracked_order_ref() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(), lmt_price: 336.0,
        order_ref: "t1".into(), ..Default::default()
    };
    client.place_order(7, &spy(), &order).unwrap();
    let exec = crate::bridge::FillExec { client_id: 0, order_ref: String::new(), ..captured_fill_exec() };
    shared.orders.push_fill_with_exec(aapl_fill(7), exec);
    let mut w = ExecRecorder::default();
    client.process_msgs(&mut w);
    assert_eq!(w.execs[0].1.order_ref, "t1");
    assert_eq!(w.execs[0].1.client_id, 0, "the Rust client has no client id");
}

fn filter_count(client: &EClient, filter: crate::api::types::ExecutionFilter) -> usize {
    let mut w = ExecRecorder::default();
    client.req_executions(3, &filter, &mut w);
    w.execs.len()
}

#[test]
fn req_executions_filter_matches_like_the_reference() {
    use crate::api::types::ExecutionFilter;
    let (client, _rx, shared) = test_client();
    shared.orders.push_fill_with_exec(aapl_fill(7), captured_fill_exec());
    client.process_msgs(&mut RecordingWrapper::default());

    assert_eq!(filter_count(&client, ExecutionFilter { side: "BUY".into(), ..Default::default() }), 1);
    assert_eq!(filter_count(&client, ExecutionFilter { side: "BOT".into(), ..Default::default() }), 1);
    assert_eq!(filter_count(&client, ExecutionFilter { side: "SELL".into(), ..Default::default() }), 0);
    assert_eq!(filter_count(&client, ExecutionFilter { client_id: 250, ..Default::default() }), 1);
    assert_eq!(filter_count(&client, ExecutionFilter { client_id: 251, ..Default::default() }), 0);
    assert_eq!(filter_count(&client, ExecutionFilter { exchange: "ARCA".into(), ..Default::default() }), 1);
    assert_eq!(filter_count(&client, ExecutionFilter { exchange: "arca".into(), ..Default::default() }), 0, "exact match");
    // Time: executions at or after the filter time.
    assert_eq!(filter_count(&client, ExecutionFilter { time: "20260925 04:49:54 US/Eastern".into(), ..Default::default() }), 1);
    assert_eq!(filter_count(&client, ExecutionFilter { time: "20260925-08:49:55".into(), ..Default::default() }), 0);
}

// ═══════════════════════════════════════════════════════════════════
//  open_order + order_status on every report (ibx#473)
// ═══════════════════════════════════════════════════════════════════

#[derive(Default)]
struct StatusRecorder { events: Vec<String> }
impl Wrapper for StatusRecorder {
    fn open_order(&mut self, order_id: i64, _c: &Contract, order: &Order, state: &crate::api::types::OrderState) {
        self.events.push(format!("open_order:{order_id}:{}:{}", state.status, order.order_ref));
    }
    fn order_status(&mut self, order_id: i64, status: &str, filled: f64, _r: f64, _a: f64,
                    _p: i64, _pa: i64, last_fill_price: f64, client_id: i64, _w: &str, _m: f64) {
        self.events.push(format!("order_status:{order_id}:{status}:{filled}:{last_fill_price}:{client_id}"));
    }
}

fn update(order_id: u64, status: OrderStatus, filled: i64) -> OrderUpdate {
    OrderUpdate {
        order_id, instrument: 0, status,
        filled_qty_fixed: filled * crate::types::QTY_SCALE,
        remaining_qty_fixed: (100 - filled) * crate::types::QTY_SCALE,
        avg_fill_price: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    }
}

fn placed_order(client: &EClient, shared: &Arc<SharedState>, id: i64) {
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(), lmt_price: 336.0,
        order_ref: "ref1".into(), ..Default::default()
    };
    client.place_order(id, &spy(), &order).unwrap();
}

// Every report of a known order gives open_order then order_status, also
// when the status is unchanged (a modify confirm).
#[test]
fn every_report_gives_open_order_then_order_status() {
    let (client, _rx, shared) = test_client();
    placed_order(&client, &shared, 60);
    shared.orders.push_order_update(update(60, OrderStatus::Submitted, 0));
    shared.orders.push_order_update(update(60, OrderStatus::Submitted, 0));
    let mut w = StatusRecorder::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, [
        "open_order:60:Submitted:ref1", "order_status:60:Submitted:0:0:0",
        "open_order:60:Submitted:ref1", "order_status:60:Submitted:0:0:0",
    ]);
}

#[test]
fn a_cancel_gives_order_status_only() {
    let (client, _rx, shared) = test_client();
    placed_order(&client, &shared, 61);
    shared.orders.push_order_update(update(61, OrderStatus::Cancelled, 0));
    let mut w = StatusRecorder::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["order_status:61:Cancelled:0:0:0"]);
}

// A fill gives open_order too; a later report keeps the last fill price.
#[test]
fn a_later_report_carries_the_last_fill_price() {
    let (client, _rx, shared) = test_client();
    placed_order(&client, &shared, 62);
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 62, side: Side::Buy, price: 336 * PRICE_SCALE,
        qty_fixed: 40 * crate::types::QTY_SCALE, remaining_fixed: 60 * crate::types::QTY_SCALE,
        cum_qty_fixed: 40 * crate::types::QTY_SCALE, avg_price: 336 * PRICE_SCALE, commission: 0, timestamp_ns: 0,
    });
    shared.orders.push_order_update(update(62, OrderStatus::PartiallyFilled, 40));
    let mut w = StatusRecorder::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, [
        "open_order:62:Submitted:ref1", "order_status:62:Submitted:40:336:0",
        "open_order:62:Submitted:ref1", "order_status:62:Submitted:40:336:0",
    ]);
}

// An order this client does not know gives order_status only.
#[test]
fn a_report_of_an_unknown_order_gives_order_status_only() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_order_update(update(63, OrderStatus::Submitted, 0));
    let mut w = StatusRecorder::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["order_status:63:Submitted:0:0:0"]);
}

// ═══════════════════════════════════════════════════════════════════
//  Account updates (ibx#475)
// ═══════════════════════════════════════════════════════════════════

#[derive(Default)]
struct AccountRec { events: Vec<String> }
impl Wrapper for AccountRec {
    fn update_account_value(&mut self, key: &str, value: &str, currency: &str, _a: &str) {
        self.events.push(format!("value:{key}:{value}:{currency}"));
    }
    fn update_portfolio(&mut self, c: &Contract, position: f64, _mp: f64, _mv: f64, _ac: f64, _u: f64, _r: f64, _a: &str) {
        self.events.push(format!("portfolio:{}:{}:{}", c.con_id, position, c.primary_exchange));
    }
    fn update_account_time(&mut self, time: &str) {
        self.events.push(format!("time:{time}"));
    }
    fn account_download_end(&mut self, _a: &str) {
        self.events.push("end".into());
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        self.events.push(format!("error:{id}:{code}:{msg}"));
    }
}

/// Rows as the server sent them at 12:16:29 UTC (08:16 US/Eastern), then
/// the end marker when `complete`.
fn seed_account_rows(shared: &SharedState, complete: bool) {
    shared.portfolio.update_account_rows(|store| {
        store.set("AccountType", "", "INDIVIDUAL");
        store.set("NetLiquidation", "USD", "953633.06");
        store.set("CashBalance", "BASE", "899133.4993");
        store.time_secs = 1790338589;
        store.image_complete = complete;
    });
}

// The first image: every value as sent, the portfolio rows each followed by
// the time, the time, then the end, once.
#[test]
fn account_updates_send_the_image_then_the_end_once() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.set_position_info(crate::types::PositionInfo {
        con_id: 756733, position_fixed: 18 * crate::types::QTY_SCALE, symbol: "SPY".into(),
        sec_type: "STK".into(), currency: "USD".into(), ..Default::default()
    });
    seed_account_rows(&shared, false);
    client.req_account_updates(true, "");
    let mut w = AccountRec::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "no image before the server's end marker: {:?}", w.events);

    shared.portfolio.update_account_rows(|s| s.image_complete = true);
    let mut w = AccountRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, [
        "value:AccountType:INDIVIDUAL:",
        "value:NetLiquidation:953633.06:USD",
        "value:CashBalance:899133.4993:BASE",
        "portfolio:756733:18:",
        "time:08:16",
        "time:08:16",
        "end",
    ]);

    // A periodic batch: the changed value only, the time, no end.
    shared.portfolio.update_account_rows(|s| {
        s.set("NetLiquidation", "USD", "953642.02");
        s.set("CashBalance", "BASE", "899133.4993");
        s.time_secs = 1790338657;
    });
    let mut w = AccountRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["value:NetLiquidation:953642.02:USD", "time:08:17"]);

    // Nothing changed: nothing sent.
    let mut w = AccountRec::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);
}

// A second subscribe while subscribed sends nothing: no image, no end.
#[test]
fn a_second_subscribe_sends_nothing() {
    let (client, _rx, shared) = test_client();
    seed_account_rows(&shared, true);
    client.req_account_updates(true, "");
    client.process_msgs(&mut AccountRec::default());
    client.req_account_updates(true, "");
    let mut w = AccountRec::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);
}

#[test]
fn an_unsubscribe_answers_2100_and_a_new_subscribe_gets_the_image_again() {
    let (client, _rx, shared) = test_client();
    seed_account_rows(&shared, true);
    client.req_account_updates(true, "");
    client.process_msgs(&mut AccountRec::default());
    client.req_account_updates(false, "");
    let mut w = AccountRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["error:-1:2100:API client has been unsubscribed from account data."]);

    client.req_account_updates(true, "");
    let mut w = AccountRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events.last().map(String::as_str), Some("end"));
    assert_eq!(w.events.iter().filter(|e| e.starts_with("value:")).count(), 3);
}

#[test]
fn an_unsubscribe_when_not_subscribed_sends_nothing() {
    let (client, _rx, _shared) = test_client();
    client.req_account_updates(false, "");
    let mut w = AccountRec::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);
}

// ═══════════════════════════════════════════════════════════════════
//  Account summary subscription (ibx#479)
// ═══════════════════════════════════════════════════════════════════

#[derive(Default)]
struct SummaryRec { events: Vec<String> }
impl Wrapper for SummaryRec {
    fn account_summary(&mut self, req_id: i64, _a: &str, tag: &str, value: &str, currency: &str) {
        self.events.push(format!("row:{req_id}:{tag}:{value}:{currency}"));
    }
    fn account_summary_end(&mut self, req_id: i64) {
        self.events.push(format!("end:{req_id}"));
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        self.events.push(format!("error:{id}:{code}:{msg}"));
    }
}

fn summary_sent(rx: &crossbeam_channel::Receiver<ControlCommand>) -> Vec<String> {
    rx.try_iter().filter_map(|c| match c {
        ControlCommand::SubscribeAccountSummary { sr_id, tags, group } => Some(format!("sub:{sr_id}:{tags}:{group}")),
        ControlCommand::CancelAccountSummary { sr_id } => Some(format!("cancel:{sr_id}")),
        _ => None,
    }).collect()
}

fn summary_row(key: &str, value: &str, currency: &str) -> crate::bridge::AccountRow {
    crate::bridge::AccountRow { key: key.into(), value: value.into(), currency: currency.into(), ledger: false }
}

#[test]
fn account_summary_is_a_server_subscription() {
    let (client, rx, shared) = test_client();
    client.req_account_summary(1, "All", "AccountType,NetLiquidation,$LEDGER:USD");
    assert_eq!(summary_sent(&rx), ["sub:SR.Socket.1:AccountType,NetLiquidation,$LEDGER:All"]);

    // Rows as sent; ledger rows of the chosen currency only; the end.
    shared.portfolio.push_account_summary_event(crate::bridge::AccountSummaryEvent {
        sr_id: "SR.Socket.1".into(), ledger: false, end: false,
        rows: vec![summary_row("AccountType", "INDIVIDUAL", ""), summary_row("NetLiquidation", "953633.06", "USD")],
    });
    shared.portfolio.push_account_summary_event(crate::bridge::AccountSummaryEvent {
        sr_id: "SR.Socket.1".into(), ledger: true, end: false,
        rows: vec![summary_row("CashBalance", "899133.4993", "BASE"), summary_row("CashBalance", "899133.4993", "USD")],
    });
    shared.portfolio.push_account_summary_event(crate::bridge::AccountSummaryEvent {
        sr_id: "SR.Socket.1".into(), ledger: false, end: true, rows: vec![],
    });
    let mut w = SummaryRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, [
        "row:1:AccountType:INDIVIDUAL:",
        "row:1:NetLiquidation:953633.06:USD",
        "row:1:CashBalance:899133.4993:USD",
        "end:1",
    ]);

    // A later batch keeps coming, with its own end.
    shared.portfolio.push_account_summary_event(crate::bridge::AccountSummaryEvent {
        sr_id: "SR.Socket.1".into(), ledger: false, end: false,
        rows: vec![summary_row("NetLiquidation", "953642.02", "USD")],
    });
    shared.portfolio.push_account_summary_event(crate::bridge::AccountSummaryEvent {
        sr_id: "SR.Socket.1".into(), ledger: false, end: true, rows: vec![],
    });
    let mut w = SummaryRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["row:1:NetLiquidation:953642.02:USD", "end:1"]);

    // Cancel: the server subscription is cancelled; later rows are dropped.
    client.cancel_account_summary(1);
    assert_eq!(summary_sent(&rx), ["cancel:SR.Socket.1"]);
    shared.portfolio.push_account_summary_event(crate::bridge::AccountSummaryEvent {
        sr_id: "SR.Socket.1".into(), ledger: false, end: true, rows: vec![],
    });
    let mut w = SummaryRec::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);
}

#[test]
fn account_summary_refusals_and_limit() {
    let (client, rx, _shared) = test_client();
    client.req_account_summary(1, "All", "");
    client.req_account_summary(2, "", "NetLiquidation");
    client.req_account_summary(3, "all", "NetLiquidation");
    client.req_account_summary(4, "All", "NetLiquidation");
    client.req_account_summary(5, "AllNonProp", "NetLiquidation");
    client.req_account_summary(6, "All", "NetLiquidation");
    // The same id again replaces its request: cancel, then subscribe.
    client.req_account_summary(5, "All", "Cushion");
    let mut w = SummaryRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, [
        "error:1:321:Error validating request.-'b2' : cause - Tags cannot be null",
        "error:2:321:Error validating request.-'b2' : cause - Group name cannot be null",
        "error:3:321:Error validating request.-'b2' : cause - Group name is invalid",
        "error:6:322:Maximum number of account summary requests exceeded; desubscribe to previous request first",
    ]);
    assert_eq!(summary_sent(&rx), [
        "sub:SR.Socket.1:NetLiquidation:All",
        "sub:SR.Socket.2:NetLiquidation:AllNonProp",
        "cancel:SR.Socket.2",
        "sub:SR.Socket.3:Cushion:All",
    ]);
}

// ═══════════════════════════════════════════════════════════════════
//  req_positions is a subscription (ibx#477)
// ═══════════════════════════════════════════════════════════════════

fn aapl_position(qty: i64, avg: i64) -> PositionInfo {
    PositionInfo {
        con_id: 265598, position_fixed: qty * crate::types::QTY_SCALE, avg_cost: avg,
        symbol: "AAPL".into(), sec_type: "STK".into(), currency: "USD".into(), ..Default::default()
    }
}

// The capture of 25/09/2026: after a fill of BUY 100 AAPL for another
// client, the observer got a position row, then a second one when the
// average cost moved, with no new req_positions.
#[test]
fn req_positions_sends_a_row_on_each_change() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.set_account_download_complete();
    let mut w = RecordingWrapper::default();
    client.req_positions(&mut w);
    assert_eq!(w.events, vec!["position_end"]);

    shared.portfolio.set_position_info(aapl_position(100, 33625 * PRICE_SCALE / 100));
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events.iter().filter(|e| e.starts_with("position:")).count(), 1, "{:?}", w.events);
    assert!(!w.events.iter().any(|e| e == "position_end"), "no end after the snapshot");

    // Average cost moves: another row. Same values again: nothing.
    shared.portfolio.set_position_info(aapl_position(100, 336_260_003 * PRICE_SCALE / 1_000_000));
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events.iter().filter(|e| e.starts_with("position:")).count(), 1);
    shared.portfolio.set_position_info(aapl_position(100, 336_260_003 * PRICE_SCALE / 1_000_000));
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);

    // After cancel_positions: no row.
    client.cancel_positions();
    shared.portfolio.set_position_info(aapl_position(200, 336 * PRICE_SCALE));
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);
}

// Before the position data is in, req_positions returns at once; the
// snapshot and the end come through process_msgs.
#[test]
fn req_positions_does_not_wait_in_the_caller() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.set_position_info(aapl_position(100, 336 * PRICE_SCALE));
    let started = std::time::Instant::now();
    let mut w = RecordingWrapper::default();
    client.req_positions(&mut w);
    assert!(started.elapsed() < std::time::Duration::from_millis(100));
    assert!(w.events.is_empty(), "{:?}", w.events);

    shared.portfolio.set_account_download_complete();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events.last().map(String::as_str), Some("position_end"));
    assert_eq!(w.events.iter().filter(|e| e.starts_with("position:")).count(), 1);
}

// No position data after 30 s: error 2151 and no end, as the reference.
#[test]
fn req_positions_gives_2151_when_the_data_never_comes() {
    let (client, _rx, _shared) = test_client();
    client.req_positions(&mut RecordingWrapper::default());
    if let Some(sub) = client.core.positions_sub.lock().unwrap().as_mut() {
        sub.backdate(crate::client_core::POSITIONS_WAIT);
    }
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, vec!["error:-1:2151:Positions info is not available yet"]);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "the request ended");
}

// ═══════════════════════════════════════════════════════════════════
//  Multi-account requests (ibx#476)
// ═══════════════════════════════════════════════════════════════════

#[derive(Default)]
struct MultiRec { events: Vec<String> }
impl Wrapper for MultiRec {
    fn account_update_multi(&mut self, req_id: i64, _a: &str, model: &str, key: &str, value: &str, currency: &str) {
        self.events.push(format!("acct:{req_id}:{model}:{key}:{value}:{currency}"));
    }
    fn account_update_multi_end(&mut self, req_id: i64) {
        self.events.push(format!("acct_end:{req_id}"));
    }
    fn position_multi(&mut self, req_id: i64, _a: &str, model: &str, c: &Contract, pos: f64, _avg: f64) {
        self.events.push(format!("pos:{req_id}:{model}:{}:{pos}", c.con_id));
    }
    fn position_multi_end(&mut self, req_id: i64) {
        self.events.push(format!("pos_end:{req_id}"));
    }
    fn update_account_value(&mut self, _k: &str, _v: &str, _c: &str, _a: &str) {
        self.events.push("single:update_account_value".into());
    }
    fn account_download_end(&mut self, _a: &str) {
        self.events.push("single:account_download_end".into());
    }
    fn position(&mut self, _a: &str, _c: &Contract, _p: f64, _avg: f64) {
        self.events.push("single:position".into());
    }
    fn position_end(&mut self) {
        self.events.push("single:position_end".into());
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        self.events.push(format!("error:{id}:{code}:{msg}"));
    }
}

fn seed_multi_account(shared: &SharedState) {
    shared.portfolio.update_account_rows(|store| {
        store.set_row("NetLiquidation", "USD", "953633.06", false);
        store.set_row("CashBalance", "BASE", "899133.4993", true);
        store.image_complete = true;
    });
}

// A key sent by the account frame and by the ledger (AccruedCash USD, seen
// on paper) is a ledger key: ledgerAndNLV includes it.
#[test]
fn a_key_also_in_the_ledger_is_a_ledger_key() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.update_account_rows(|store| {
        store.set_row("AccruedCash", "USD", "1893.50", false);
        store.set_row("AccruedCash", "USD", "1893.5", true);
        store.image_complete = true;
    });
    let mut w = MultiRec::default();
    client.req_account_updates_multi(9002, "", "", true, &mut w);
    assert_eq!(w.events, ["acct:9002::AccruedCash:1893.5:USD", "acct_end:9002"]);
}

#[test]
fn account_updates_multi_carries_its_request_id_and_model_code() {
    let (client, _rx, shared) = test_client();
    seed_multi_account(&shared);
    let mut w = MultiRec::default();
    client.req_account_updates_multi(9001, "", "Core", false, &mut w);
    client.req_account_updates_multi(9002, "", "", true, &mut w);
    client.req_account_updates_multi(9001, "", "", false, &mut w);
    assert_eq!(w.events, [
        "acct:9001:Core:NetLiquidation:953633.06:USD",
        "acct:9001:Core:CashBalance:899133.4993:BASE",
        "acct_end:9001",
        "acct:9002::CashBalance:899133.4993:BASE",
        "acct_end:9002",
        "error:9001:322:Duplicate ticker id",
    ]);

    // A change: a row for each request that has the key, no end.
    shared.portfolio.update_account_rows(|s| { s.set_row("NetLiquidation", "USD", "953642.02", false); });
    let mut w = MultiRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["acct:9001:Core:NetLiquidation:953642.02:USD"]);

    // Cancelled: nothing more.
    client.cancel_account_updates_multi(9001);
    shared.portfolio.update_account_rows(|s| { s.set_row("NetLiquidation", "USD", "1", false); });
    let mut w = MultiRec::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);
}

#[test]
fn positions_multi_carries_its_request_id_and_updates() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.set_account_download_complete();
    shared.portfolio.set_position_info(aapl_position(100, 336 * PRICE_SCALE));
    let mut w = MultiRec::default();
    client.req_positions_multi(9003, "", "Core", &mut w);
    assert_eq!(w.events, ["pos:9003:Core:265598:100", "pos_end:9003"]);

    shared.portfolio.set_position_info(aapl_position(101, 336 * PRICE_SCALE));
    let mut w = MultiRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["pos:9003:Core:265598:101"]);

    client.cancel_positions_multi(9003);
    shared.portfolio.set_position_info(aapl_position(102, 336 * PRICE_SCALE));
    let mut w = MultiRec::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);
}

// ═══════════════════════════════════════════════════════════════════
//  Quotes ibx subscribes to for the P&L
// ═══════════════════════════════════════════════════════════════════

/// Answer the engine side of the internal subscriptions sent so far with
/// instrument `iid`; returns the conIds subscribed and unsubscribed.
fn answer_pnl_quotes(rx: &crossbeam_channel::Receiver<ControlCommand>, iid: InstrumentId) -> (Vec<i64>, Vec<InstrumentId>) {
    let (mut subs, mut unsubs) = (Vec::new(), Vec::new());
    for cmd in rx.try_iter() {
        match cmd {
            ControlCommand::Subscribe { con_id, reply_tx: Some(tx), .. } => {
                let _ = tx.send(Ok(iid));
                subs.push(con_id);
            }
            ControlCommand::Unsubscribe { instrument } => unsubs.push(instrument),
            _ => {}
        }
    }
    (subs, unsubs)
}

#[test]
fn a_pnl_request_subscribes_the_quotes_it_needs_and_cancels_them_after() {
    let (client, rx, shared) = test_client();
    client.core.con_id_to_instrument.lock().unwrap().clear();
    shared.portfolio.set_position_info(aapl_position(100, 336 * PRICE_SCALE));
    client.req_pnl(1, "DU123", "");
    client.process_msgs(&mut RecordingWrapper::default());
    let (subs, _) = answer_pnl_quotes(&rx, 7);
    assert_eq!(subs, [265598], "the held position's quote");

    // The reply is read on the next pass; the quote feeds the P&L only.
    client.process_msgs(&mut RecordingWrapper::default());
    assert_eq!(client.core.con_id_to_instrument.lock().unwrap().get(&265598), Some(&7));
    assert!(client.core.instrument_to_req.lock().unwrap().get(&7).is_none(), "no tick callbacks");

    // No P&L request left: the quote is cancelled.
    client.cancel_pnl(1);
    if let Some(q) = client.core.pnl_quotes.lock().unwrap().checked_at_mut() { *q -= std::time::Duration::from_secs(2); }
    client.process_msgs(&mut RecordingWrapper::default());
    let (_, unsubs) = answer_pnl_quotes(&rx, 7);
    assert_eq!(unsubs, [7]);
    assert!(client.core.con_id_to_instrument.lock().unwrap().get(&265598).is_none());
}

#[test]
fn a_pnl_quote_becomes_the_callers_subscription() {
    let (client, rx, shared) = test_client();
    client.core.con_id_to_instrument.lock().unwrap().clear();
    shared.portfolio.set_position_info(aapl_position(100, 336 * PRICE_SCALE));
    client.req_pnl(1, "DU123", "");
    client.process_msgs(&mut RecordingWrapper::default());
    answer_pnl_quotes(&rx, 7);
    client.process_msgs(&mut RecordingWrapper::default());

    let aapl = Contract { con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), ..Default::default() };
    client.req_mkt_data(5, &aapl, "", false, false).unwrap();
    let (subs, _) = answer_pnl_quotes(&rx, 7);
    assert!(subs.is_empty(), "no second subscription to the server");
    assert_eq!(client.core.instrument_to_req.lock().unwrap().get(&7), Some(&5));

    // The P&L ends: the caller's subscription stays.
    client.cancel_pnl(1);
    if let Some(q) = client.core.pnl_quotes.lock().unwrap().checked_at_mut() { *q -= std::time::Duration::from_secs(2); }
    client.process_msgs(&mut RecordingWrapper::default());
    let (_, unsubs) = answer_pnl_quotes(&rx, 7);
    assert!(unsubs.is_empty());
}

#[test]
fn no_internal_quote_when_the_caller_has_one() {
    let (client, rx, shared) = test_client();
    shared.portfolio.set_position_info(aapl_position(100, 336 * PRICE_SCALE));
    client.core.con_id_to_instrument.lock().unwrap().insert(265598, 3);
    client.core.instrument_to_req.lock().unwrap().insert(3, 9);
    client.req_pnl(1, "DU123", "");
    client.process_msgs(&mut RecordingWrapper::default());
    let (subs, _) = answer_pnl_quotes(&rx, 3);
    assert!(subs.is_empty());
}
