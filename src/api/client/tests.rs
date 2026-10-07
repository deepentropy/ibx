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
        farm_host: String::new(),
        farm_name: String::new(),
        session_epoch: String::new(),
        ns_secure_refused: false,
        use_ssl: true,
        ssl_farms: String::new(),
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
// The API's Contract has no default security type or exchange: the
// contract names them, as an order needs its exchange.
fn spy() -> Contract {
    Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), ..Default::default() }
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

// ibx#466: 32-bit ids, as the reference's: a client with no order known
// starts at 1; each next_order_id is one above the last.
#[test]
fn next_order_id_monotonic() {
    let (client, _rx, _shared) = test_client();
    let id1 = client.next_order_id();
    let id2 = client.next_order_id();
    let id3 = client.next_order_id();
    assert_eq!((id1, id2, id3), (1, 2, 3));
}

// ibx#466: reqIds gives the highest order id the client used + 1, the ids
// the server's reports gave for its client id included; it reserves
// nothing.
#[test]
fn req_ids_calls_wrapper() {
    let (client, rx, shared) = test_client();
    let mut w = RecordingWrapper::default();
    client.req_ids(&mut w);
    assert_eq!(w.events, ["next_valid_id:1"]);
    shared.orders.note_reported_order_id(0, 68);
    client.req_ids(&mut w);
    client.req_ids(&mut w);
    shared.market.set_instrument_count(1);
    let lmt = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 1.0, ..Default::default() };
    client.place_order(80, &spy(), &lmt).unwrap();
    while rx.try_recv().is_ok() {}
    client.req_ids(&mut w);
    assert_eq!(w.events, ["next_valid_id:1", "next_valid_id:69", "next_valid_id:69", "next_valid_id:81"]);
    assert_eq!(client.next_order_id(), 81);
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

// ibx#278: a contract with no conId is no identity: each symbol-only
// request goes to the engine for a lookup, with its currency and contract
// fields, and is never refused by the conId duplicate guard.
#[test]
fn symbol_only_requests_are_looked_up_not_keyed_on_con_id_0() {
    let (client, rx, _shared) = test_client();
    // A live request under conId 0, as the old keying left it.
    client.core.con_id_to_instrument.lock().unwrap().insert(0, 3);
    client.core.instrument_to_req.lock().unwrap().insert(3, vec![1]);
    for (req, symbol) in [(2, "QQQ"), (4, "SPY")] {
        let c = Contract { symbol: symbol.into(), sec_type: "STK".into(), exchange: "SMART".into(),
            currency: "USD".into(), primary_exchange: "NASDAQ".into(), ..Default::default() };
        let r = client.req_mkt_data(req, &c, "", false, false);
        assert!(!r.as_ref().is_err_and(|e| e.contains("already has a live")), "{r:?}");
    }
    let sent: Vec<(String, String, String)> = rx.try_iter().map(|c| match c {
        ControlCommand::SubscribeBySymbol { symbol, currency, filters, .. } => (symbol, currency, filters.primary_exchange),
        other => panic!("expected SubscribeBySymbol, got {:?}", other),
    }).collect();
    assert_eq!(sent, [("QQQ".into(), "USD".into(), "NASDAQ".into()), ("SPY".into(), "USD".into(), "NASDAQ".into())]);
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

// ibx#444: a second request id on a contract this client streams shares
// its subscription, as the reference: nothing reaches the engine, and both
// requests stay mapped (ibx#233 is kept: none is orphaned).
#[test]
fn req_mkt_data_second_request_on_a_contract_shares_it() {
    let (client, rx, _shared) = test_client();
    // Existing live subscription for SPY (instrument 0) under req_id 1.
    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, vec![1]);

    client.req_mkt_data(2, &spy(), "", false, false).unwrap();
    assert!(rx.try_recv().is_err(), "nothing may reach the engine");
    assert_eq!(client.core.instrument_to_req.lock().unwrap().get(&0), Some(&vec![1, 2]));
    assert_eq!(client.core.req_to_instrument.lock().unwrap().get(&2), Some(&0));
}

#[test]
fn cancel_mkt_data_sends_unsubscribe() {
    let (client, rx, _shared) = test_client();
    // Pre-register mapping
    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, vec![1]);
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

// ibx#455: each of the four types is asked under its own name, with the
// tick count, the size flag and the request id; another name, a combo or
// a session with tick-by-tick off is refused before anything is sent,
// with the reference's codes and texts. The limit is the engine's.
#[test]
fn req_tick_by_tick_data_types_and_local_refusals() {
    let (client, rx, shared) = test_client();
    for (req, name) in [(10, "Last"), (11, "AllLast"), (12, "BidAsk"), (13, "MidPoint")] {
        let _ = client.req_tick_by_tick_data(req, &spy(), name, 5, true);
        match rx.try_recv().unwrap() {
            ControlCommand::SubscribeTbt { req_id, tbt_type, number_of_ticks, ignore_size, .. } => {
                assert_eq!((req_id, tbt_type.as_str(), number_of_ticks, ignore_size), (req, name, 5, true));
            }
            other => panic!("expected SubscribeTbt, got {other:?}"),
        }
    }
    let _ = client.req_tick_by_tick_data(20, &spy(), "last", 0, false);
    let bag = Contract { sec_type: "BAG".into(), ..spy() };
    let _ = client.req_tick_by_tick_data(21, &bag, "Last", 0, false);
    shared.reference.set_tick_by_tick_limits(3, true);
    let _ = client.req_tick_by_tick_data(22, &spy(), "AllLast", 0, false);
    assert!(rx.try_recv().is_err(), "nothing sent for a refused request");

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    for e in [
        "error:20:321:Error validating request.-'bT' : cause - Tick-by-tick data type is incorrect/not set.",
        "error:21:321:Error validating request.-'bT' : cause - 'BAG' security type is not supported in ReqTickByTick(97) request",
        "error:22:10189:Failed to request tick-by-tick data.AllLast tick-by-tick requests are not supported for SPY",
    ] {
        assert!(w.events.contains(&e.to_string()), "{e}: {:?}", w.events);
    }
}

fn tbt_trade(req_id: i64, tbt_type: TbtType, past_limit: bool, unreported: bool) -> TbtTrade {
    TbtTrade {
        instrument: 0, req_id, tbt_type, price: 100 * PRICE_SCALE, size: 3, timestamp: 1,
        exchange: "ARCA".into(), conditions: String::new(), past_limit, unreported,
    }
}

// ibx#404, ibx#455: each tick goes to the request the engine names, with
// tickType 1 for a Last request and 2 for AllLast and the entry's
// attribute mask; midpoints go to tickByTickMidPoint; an error the engine
// gives ends the request with its code and text.
#[test]
fn tick_by_tick_reports_by_request_type() {
    let (client, rx, shared) = test_client();
    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, vec![1]);
    client.core.tbt_reqs.lock().unwrap().insert(7, (0, 756733, TbtType::Last));
    client.core.tbt_reqs.lock().unwrap().insert(8, (2, 265598, TbtType::AllLast));
    client.core.tbt_reqs.lock().unwrap().insert(9, (2, 265598, TbtType::BidAsk));
    client.core.tbt_reqs.lock().unwrap().insert(10, (2, 265598, TbtType::MidPoint));
    shared.market.push_tbt_trade(tbt_trade(7, TbtType::Last, true, false));
    shared.market.push_tbt_trade(tbt_trade(8, TbtType::AllLast, false, true));
    shared.market.push_tbt_quote(TbtQuote {
        instrument: 2, req_id: 9, bid: 99 * PRICE_SCALE, ask: 101 * PRICE_SCALE, bid_size: 4, ask_size: 5, timestamp: 2,
        bid_past_low: false, ask_past_high: true,
    });
    shared.market.push_tbt_mid_point(TbtMidPoint { instrument: 2, req_id: 10, mid_point: 100 * PRICE_SCALE + PRICE_SCALE / 2, timestamp: 3 });
    shared.market.push_tbt_error(9, 10189, "Failed to request tick-by-tick data.No historical market data for X".into());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    for e in [
        "tbt_last:7:1:1:100:3:ARCA:1",
        "tbt_last:8:2:1:100:3:ARCA:2",
        "tbt_bidask:9:2:99:101:4:5:2",
        "tbt_mid:10:3:100.5",
        "error:9:10189:Failed to request tick-by-tick data.No historical market data for X",
    ] {
        assert!(w.events.contains(&e.to_string()), "{e}: {:?}", w.events);
    }
    assert!(!client.core.tbt_reqs.lock().unwrap().contains_key(&9));
    assert!(rx.try_recv().is_err(), "the engine already let the refused request go");

    // Cancelling the tick-by-tick request leaves the market data mapping.
    client.cancel_tick_by_tick_data(7).unwrap();
    assert!(matches!(rx.try_recv(), Ok(ControlCommand::UnsubscribeTbt { req_id: 7 })));
    assert_eq!(client.core.req_id_for_instrument(0), 1);
    assert_eq!(client.core.con_id_to_instrument.lock().unwrap().get(&756733), Some(&0));
}

#[test]
fn cancel_tick_by_tick_data_sends_unsubscribe_tbt() {
    let (client, rx, _shared) = test_client();
    client.core.tbt_reqs.lock().unwrap().insert(10, (3, 1, TbtType::Last));
    client.cancel_tick_by_tick_data(10).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::UnsubscribeTbt { req_id: 10 }));
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
        lmt_price: 100.0, tif: "XYZ".into(), ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.to_string().contains("tif"), "got: {}", err);
    assert!(rx.try_recv().is_err());
}

// ibx#263: the reference takes all-or-none on any order type; its
// instruction field carries the type letter, then G (`jclient.pe.gI()`).
// ibx refused it on TRAIL and REL.
#[test]
fn place_order_all_or_none_trail_and_rel_are_sent() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    for (id, order_type, aux_price) in [(1, "TRAIL", 2.0), (2, "REL", 0.05)] {
        let order = Order {
            action: "SELL".into(), total_quantity: 1.0, order_type: order_type.into(),
            aux_price, all_or_none: true, ..Default::default()
        };
        client.place_order(id, &spy(), &order).unwrap();
        match rx.try_recv() {
            Ok(ControlCommand::Order(OrderRequest::SubmitEx { attrs, .. })) => assert!(attrs.all_or_none, "{order_type}"),
            other => panic!("{order_type}: {other:?}"),
        }
    }
}

// ibx#263: a trigger method the reference does not have is refused before
// sending, 321 with error 146's text (`jextend.bH.S()@2257`).
#[test]
fn place_order_unknown_trigger_method_is_refused() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    for (id, trigger_method) in [(3, -1), (4, 5), (5, 6), (6, 9)] {
        let order = Order {
            action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(),
            aux_price: 2.0, trigger_method, ..Default::default()
        };
        client.place_order(id, &spy(), &order).unwrap();
    }
    assert!(rx.try_iter().all(|c| !matches!(c, ControlCommand::Order(_))), "nothing sent");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    for id in 3..=6 {
        let want = format!("error:{id}:321:Error validating request.-'bH' : cause - Invalid trigger method");
        assert!(w.events.contains(&want), "{:?}", w.events);
    }
    for trigger_method in [0, 1, 2, 3, 4, 7, 8] {
        let order = Order {
            action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(),
            aux_price: 2.0, trigger_method, ..Default::default()
        };
        client.place_order(10 + trigger_method as i64, &spy(), &order).unwrap();
        assert!(matches!(rx.try_recv(), Ok(ControlCommand::Order(_))), "{trigger_method}");
    }
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
        ControlCommand::Order(OrderRequest::SubmitTrailingStopPct { trail_percent, .. }) => {
            assert_eq!(trail_percent, 5 * crate::types::PRICE_SCALE); // 5.0%
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
        lmt_price: 148.0, aux_price: 2.0, trail_stop_price: 150.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    // lmtPrice alone is an absolute limit price, not an offset (ib-agent#194).
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitTrailingStopLimit { lmt_price, lmt_offset, .. }) => {
            assert_eq!(lmt_price, Some(148 * PRICE_SCALE));
            assert_eq!(lmt_offset, 0);
        }
        other => panic!("expected SubmitTrailingStopLimit, got {:?}", other),
    }
}

// ib-agent#194: a TRAIL LIMIT without trailStopPrice is refused first, with
// the reference's text (no final period); nothing is sent.
// ibx#481: requestFA and replaceFA on a session that is not FA get the
// reference's error 321; nothing waits forever.
#[test]
fn fa_requests_on_a_non_fa_session_are_refused() {
    let (client, _rx, shared) = test_client();
    client.request_fa(1);
    client.replace_fa(5, 1, "<ListOfGroups/>");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "error:2147483647:321:Error validating request.-'b9' : cause - FA data operations ignored for non FA customers."), "{:?}", w.events);
    assert!(w.events.iter().any(|e| e == "error:5:321:Error validating request.-'b1' : cause - FA data operations ignored for non FA customers."), "{:?}", w.events);

    // On an FA session there is no such error.
    shared.reference.set_fa_session(true);
    client.request_fa(1);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.contains(":321:")), "{:?}", w.events);
}

// ibx#443: the WSH requests get the reference's permission error from the
// logon's news sources: 10276 without the source, 10277 when it is listed
// without a subscription. With the permission, the errors of a request
// that cannot be served. The cancels answer nothing.
#[test]
fn wsh_requests_get_the_permission_error() {
    use crate::api::types::WshEventData;
    let (client, rx, shared) = test_client();
    let answers = |client: &EClient| {
        client.req_wsh_meta_data(1);
        client.req_wsh_event_data(2, &WshEventData { con_id: 265598, ..Default::default() });
        client.cancel_wsh_meta_data(1);
        client.cancel_wsh_event_data(2);
        let mut w = RecordingWrapper::default();
        client.process_msgs(&mut w);
        w.events.into_iter().filter(|e| e.starts_with("error:")).collect::<Vec<_>>()
    };

    // The captured paper logon (ibx#460): no such source.
    shared.reference.set_news_sources("BRFG,BRFUPDN,DJ-N,DJNL".split(',').map(String::from).collect());
    shared.reference.set_news_sources_unsubscribed("BZ,DJTOP,FLY".split(',').map(String::from).collect());
    assert_eq!(answers(&client), [
        "error:1:10276:News feed is not allowed.",
        "error:2:10276:News feed is not allowed.",
    ]);

    shared.reference.set_news_sources_unsubscribed(vec!["BZ".into(), "WSHE".into()]);
    assert_eq!(answers(&client), [
        "error:1:10277:News Feed requires permissions. Please login to Portal to subscribe.",
        "error:2:10277:News Feed requires permissions. Please login to Portal to subscribe.",
    ]);

    shared.reference.set_news_sources(vec!["BRFG".into(), "wshe".into()]);
    assert_eq!(answers(&client), [
        "error:1:10279:Failed to request WSH meta data.The request is not supported.",
        "error:2:10282:WSH meta data not requested.",
    ]);
    assert!(rx.try_recv().is_err(), "nothing sent");

    let unset = WshEventData::default();
    assert_eq!((unset.con_id, unset.total_limit), (i32::MAX, i32::MAX));
}

// ibx#444: a market data request id already live gets 322, a cancel of an
// unknown id gets 300, as the reference.
#[test]
fn market_data_duplicate_id_and_unknown_cancel() {
    let (client, rx, _shared) = test_client();
    client.core.req_to_instrument.lock().unwrap().insert(7, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, vec![7]);
    client.req_mkt_data(7, &spy(), "", false, false).unwrap();
    assert!(rx.try_recv().is_err(), "nothing sent");
    client.cancel_mkt_data(99).unwrap();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.contains(&"error:7:322:Error processing request.-'bQ' : cause - Duplicate ticker id".to_string()), "{:?}", w.events);
    assert!(w.events.contains(&"error:99:300:Can't find EId with tickerId:99".to_string()), "{:?}", w.events);
}

// ibx#447 / ibx#444: a reject reaches the client as the reference reports
// it: delayed -> market_data_type 3 then 10167, the subscription stays;
// not subscribed -> 354 and the subscription is gone.
#[test]
fn market_data_rejects_are_reported() {
    let (client, rx, shared) = test_client();
    for (req, inst) in [(5i64, 0u32), (6, 1)] {
        client.core.req_to_instrument.lock().unwrap().insert(req, inst);
        client.core.instrument_to_req.lock().unwrap().insert(inst, vec![req]);
    }
    shared.market.push_md_reject(crate::bridge::MdReject::Delayed { instrument: 0 });
    shared.market.push_md_reject(crate::bridge::MdReject::NotSubscribed {
        instrument: 1, delayed_available: false, needs_api_subscription: false, description: String::new(), kept_params: None });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let at = |e: &str| w.events.iter().position(|x| x == e);
    let mdt = at("market_data_type:5:3").expect("type 3");
    let delayed = at("error:5:10167:Requested market data is not subscribed. Displaying delayed market data.").expect("10167");
    assert!(mdt < delayed, "{:?}", w.events);
    assert!(at(&format!("error:6:354:{}", crate::client_core::MD_NOT_SUBSCRIBED)).is_some(), "{:?}", w.events);
    assert!(client.core.req_to_instrument.lock().unwrap().contains_key(&5));
    assert!(client.core.delayed_reqs.lock().unwrap().contains(&5), "its ticks are delayed ones");
    assert_eq!(crate::client_core::delayed_tick_type(1), 66);
    assert_eq!(crate::client_core::delayed_tick_type(14), 76);
    assert_eq!(crate::client_core::delayed_tick_type(45), 88);
    assert!(!client.core.req_to_instrument.lock().unwrap().contains_key(&6));
    assert!(rx.try_iter().any(|c| matches!(c, ControlCommand::Unsubscribe { instrument: 1 })));
}

// ibx#449: tickReqParams reaches the client once per request id, also
// when a later ack of the request (a delayed fallback) gives it again.
#[test]
fn tick_req_params_is_sent_once_per_request() {
    let (client, _rx, shared) = test_client();
    client.core.req_to_instrument.lock().unwrap().insert(5, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, vec![5]);
    let params = |min_tick: f64, bbo: &str| crate::bridge::TickReqParams {
        instrument: 0, min_tick, bbo_exchange: bbo.into(), snapshot_permissions: 3,
    };
    shared.market.push_tick_req_params(params(0.01, "9c0001"));
    shared.market.push_tick_req_params(params(0.01, "9c0001"));
    shared.market.push_tick_req_params(crate::bridge::TickReqParams { instrument: 9, ..params(0.25, "50006") });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    shared.market.push_tick_req_params(params(0.01, "9c0001"));
    client.process_msgs(&mut w);
    let sent: Vec<&String> = w.events.iter().filter(|e| e.starts_with("tick_req_params")).collect();
    assert_eq!(sent, ["tick_req_params:5:0.01:9c0001:3"], "unknown instruments give nothing");

    // A new request on the id gets its own.
    let _ = client.cancel_mkt_data(5);
    client.core.req_to_instrument.lock().unwrap().insert(5, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, vec![5]);
    shared.market.push_tick_req_params(params(0.01, "9c0001"));
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.contains(&"tick_req_params:5:0.01:9c0001:3".to_string()), "{:?}", w.events);
}

// ibx#447: market data types 1..=4 reach the engine; another value is
// refused with 321 under id -1, as the reference, and sends nothing.
#[test]
fn market_data_type_outside_one_to_four_is_refused() {
    let (client, rx, _shared) = test_client();
    for t in [1, 2, 3, 4] {
        client.req_market_data_type(t);
    }
    client.req_market_data_type(0);
    client.req_market_data_type(5);
    let sent: Vec<i32> = rx.try_iter().filter_map(|c| match c {
        ControlCommand::SetMarketDataType { market_data_type } => Some(market_data_type),
        _ => None,
    }).collect();
    assert_eq!(sent, [1, 2, 3, 4]);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let refused = "error:-1:321:Error validating request.-'b0' : cause - Invalid market data type".to_string();
    assert_eq!(w.events.iter().filter(|e| **e == refused).count(), 2, "{:?}", w.events);
}

// ibx#425: customerAccount and professionalCustomer on an account whose
// config has no CUSTACCT (the paper account) get error 145 with the
// reference's text, and nothing is sent (captured 28/09/2026).
#[test]
fn customer_account_needs_the_account_config() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    shared.reference.set_account_config(vec!["OLP".into(), "EUCOSTCALC".into(), "EUILLS".into()], String::new());
    let lmt = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 100.0, ..Default::default() };
    client.place_order(1, &spy(), &Order { customer_account: "C123".into(), ..lmt.clone() }).unwrap();
    client.place_order(2, &spy(), &Order { professional_customer: true, ..lmt.clone() }).unwrap();
    assert!(rx.try_iter().all(|c| !matches!(c, ControlCommand::Order(_))), "nothing sent");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let account = client.account_id.clone();
    assert!(w.events.iter().any(|e| *e == format!(
        "error:1:145:Error in validating entry fields -Account config doesn't allow to specify customer account value: C123 for account {account}")), "{:?}", w.events);
    assert!(w.events.iter().any(|e| *e == format!(
        "error:2:145:Error in validating entry fields -Account config doesn't allow to assign 'true' value for ProfessionalCustomer for account {account}")), "{:?}", w.events);

    // An order without these fields is not checked.
    client.place_order(3, &spy(), &lmt).unwrap();
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::Order(_)));
}

// ibx#425: on an account whose config has CUSTACCT the values go with the
// order.
#[test]
fn customer_account_is_sent_when_the_account_config_allows_it() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    shared.reference.set_account_config(vec!["CUSTACCT".into()], String::new());
    let order = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 100.0,
        customer_account: "C123".into(), professional_customer: true, ..Default::default() };
    client.place_order(1, &spy(), &order).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitLimitEx { attrs, .. }) => {
            assert_eq!(attrs.customer_account, "C123");
            assert!(attrs.professional_customer);
        }
        other => panic!("expected SubmitLimitEx, got {:?}", other),
    }
}

// ibx#307: GTX and NMIN go out as GTC and the tracked order shows GTC, as
// the reference (captured 28/09/2026: 59=1, openOrder tif GTC).
#[test]
fn gtx_and_nmin_go_out_as_gtc() {
    for tif in ["GTX", "nmin"] {
        let (client, rx, shared) = test_client();
        shared.market.set_instrument_count(1);
        let order = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
            lmt_price: 100.0, tif: tif.into(), ..Default::default() };
        client.place_order(1, &spy(), &order).unwrap();
        match rx.try_recv().unwrap() {
            ControlCommand::Order(OrderRequest::SubmitLimitEx { tif, .. }) => assert_eq!(tif, b'1'),
            other => panic!("expected SubmitLimitEx, got {:?}", other),
        }
    }
}

// ibx#469: the reference's other names for an order type give the same
// request as ibx's name, and a modify under the other name is not a type
// change.
#[test]
fn place_order_type_aliases() {
    for (alias, name) in [("STOP LIMIT", "STP LMT"), ("stplmt", "STP LMT"), ("LIMIT", "LMT"), ("MKT TO LMT", "MTL"),
                          ("PEG PRIM", "REL"), ("TRAILING STOP", "TRAIL"), ("TRAILLMT", "TRAIL LIMIT")] {
        let order = |order_type: &str| Order {
            action: "SELL".into(), total_quantity: 1.0, order_type: order_type.into(),
            lmt_price: 148.0, aux_price: 2.0, trail_stop_price: 150.0, ..Default::default()
        };
        let (client, rx, shared) = test_client();
        shared.market.set_instrument_count(1);
        client.place_order(1, &spy(), &order(alias)).unwrap();
        client.place_order(2, &spy(), &order(name)).unwrap();
        let a = format!("{:?}", rx.try_recv().unwrap()).replacen("order_id: 1", "order_id: 2", 1);
        let b = format!("{:?}", rx.try_recv().unwrap());
        assert_eq!(a, b, "{alias}");
        // Modify of order 1 under ibx's name, and of order 2 under the alias.
        client.place_order(1, &spy(), &order(name)).unwrap();
        client.place_order(2, &spy(), &order(alias)).unwrap();
        assert!(matches!(rx.try_recv().unwrap(), ControlCommand::Order(OrderRequest::Modify { .. })), "{alias}");
        assert!(matches!(rx.try_recv().unwrap(), ControlCommand::Order(OrderRequest::Modify { .. })), "{alias}");
    }
}

// ibx#469: TRAIL MIT, TRAIL LIT, PEG BEST, RPI and PASSV REL are placed,
// each as an order of its own type.
#[test]
fn place_order_trail_mit_trail_lit_peg_best_rpi_and_passv_rel() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = |order_type: &str| Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: order_type.into(),
        lmt_price: 148.0, aux_price: 2.0, trail_stop_price: 150.0, ..Default::default()
    };
    let p = |v: f64| crate::api::types::price_from_f64(v);
    for (id, order_type) in [(1, "TRAIL MIT"), (2, "TRAIL LIT"), (3, "PEG BEST"), (4, "RPI"), (5, "PASSV REL")] {
        client.place_order(id, &spy(), &order(order_type)).unwrap();
        let kind = rx.try_iter().find_map(|c| match c {
            ControlCommand::Order(OrderRequest::SubmitEx { kind, .. }) => Some(kind),
            _ => None,
        }).unwrap_or_else(|| panic!("{order_type}: no order sent"));
        match (order_type, kind) {
            ("TRAIL MIT", OrderKind::TrailMit { trail, percent: false, trail_stop_price }) =>
                assert_eq!((trail, trail_stop_price), (p(2.0), p(150.0))),
            ("TRAIL LIT", OrderKind::TrailLit { price, trail, percent: false, trail_stop_price }) =>
                assert_eq!((price, trail, trail_stop_price), (p(148.0), p(2.0), p(150.0))),
            ("PEG BEST", OrderKind::PegBest { price }) => assert_eq!(price, p(148.0)),
            ("RPI", OrderKind::Rpi { price, offset }) | ("PASSV REL", OrderKind::PassvRel { price, offset }) =>
                assert_eq!((price, offset), (p(148.0), p(2.0))),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(shared.orders.drain_order_errors().is_empty());
    // A percent takes the place of the amount.
    let pct = Order { trailing_percent: 3.0, aux_price: 0.0, ..order("TRAIL MIT") };
    client.place_order(9, &spy(), &pct).unwrap();
    assert!(rx.try_iter().any(|c| matches!(c, ControlCommand::Order(OrderRequest::SubmitEx {
        kind: OrderKind::TrailMit { percent: true, trail, .. }, .. }) if trail == p(3.0))));
    client.place_order(12, &spy(), &Order { trailing_percent: 3.0, aux_price: 0.0, ..order("TRAIL LIT") }).unwrap();
    assert!(rx.try_iter().any(|c| matches!(c, ControlCommand::Order(OrderRequest::SubmitEx {
        kind: OrderKind::TrailLit { percent: true, trail, .. }, .. }) if trail == p(3.0))));
    // A TRAIL LIT without a trigger price is refused, by amount and by
    // percent (captured 07/10/2026); a TRAIL MIT without one is sent.
    for (id, pct) in [(13, 0.0), (14, 3.0)] {
        let no_trigger = Order {
            trail_stop_price: f64::MAX, trailing_percent: pct, aux_price: if pct > 0.0 { 0.0 } else { 2.0 },
            ..order("TRAIL LIT")
        };
        client.place_order(id, &spy(), &no_trigger).unwrap();
        assert!(rx.try_iter().all(|c| !matches!(c, ControlCommand::Order(_))), "nothing sent");
        assert_eq!(shared.orders.drain_order_errors(),
            [(id, 321, "Error validating request.-'bH' : cause - Please enter a stop price".to_string())]);
    }
    client.place_order(15, &spy(), &Order { trail_stop_price: f64::MAX, ..order("TRAIL MIT") }).unwrap();
    assert!(rx.try_iter().any(|c| matches!(c, ControlCommand::Order(OrderRequest::SubmitEx {
        kind: OrderKind::TrailMit { trail_stop_price: 0, .. }, .. }))));
    // No trailing value at all.
    assert!(client.place_order(10, &spy(), &Order { aux_price: 0.0, ..order("TRAIL MIT") }).is_err());
    assert!(client.place_order(11, &spy(), &Order { aux_price: 0.0, ..order("TRAIL LIT") }).is_err());
}

// ibx#467: a goodAfterTime that is not a date and time is refused with 337
// and the reference's text; nothing is sent. A good one is sent.
#[test]
fn place_order_bad_good_after_time_is_refused() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = |gat: &str| Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 100.0,
        good_after_time: gat.into(), ..Default::default()
    };
    for (id, bad) in [(4, "tomorrow"), (5, "20261230"), (6, "20261230 25:00:00 US/Eastern"), (7, "20261230 09:30:00 Nowhere/Zone")] {
        client.place_order(id, &spy(), &order(bad)).unwrap();
    }
    assert!(rx.try_iter().all(|c| !matches!(c, ControlCommand::Order(_))), "nothing sent");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    for id in 4..=7 {
        assert!(w.events.iter().any(|e| e.starts_with(&format!(
            "error:{id}:337:Start Time: The date, time, or time-zone entered is invalid.\nThe correct format is yyyymmdd hh:mm:ss xx/xxxx\n"))),
            "{id}: {:?}", w.events);
    }

    client.place_order(8, &spy(), &order("20261230 09:30:00 US/Eastern")).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitLimitEx { attrs, .. }) => assert_eq!(attrs.good_after, 1_798_641_000),
        other => panic!("expected SubmitLimitEx, got {:?}", other),
    }
}

// ibx#335 (ib-agent#192 F6): a goodTillDate that does not parse, or whose
// zone is not UTC, this machine's zone or the contract's zone, is refused
// with 343; nothing is sent, the order does not go out without its expiry.
#[test]
fn place_order_bad_good_till_date_is_refused() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    shared.reference.cache_time_zone_id(756733, "US/Eastern");
    let order = |gtd: &str| Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 100.0,
        tif: "GTD".into(), good_till_date: gtd.into(), ..Default::default()
    };
    let machine = crate::gateway::machine_time_zone();
    let mut refused = vec![(4, "20260930 16:00:00 Nowhere/Zone"), (5, "2026093")];
    // Valid zone names, refused as in the capture; unless one is the
    // zone of the machine running the test.
    for (id, zone, gtd) in [
        (6, "America/New_York", "20260930 16:00:00 America/New_York"),
        (7, "EST5EDT", "20260930 16:00:00 EST5EDT"),
        (8, "US/Pacific", "20260930 16:00:00 US/Pacific"),
    ] {
        if machine != zone {
            refused.push((id, gtd));
        }
    }
    for (id, bad) in &refused {
        client.place_order(*id, &spy(), &order(bad)).unwrap();
    }
    assert!(rx.try_iter().all(|c| !matches!(c, ControlCommand::Order(_))), "nothing sent");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    for (id, _) in &refused {
        assert!(w.events.iter().any(|e| e.starts_with(&format!(
            "error:{id}:343:End Time: The date, time, or time-zone entered is invalid.\n"))),
            "{id}: {:?}", w.events);
    }

    // The contract's zone, and UTC: sent with the expiry in UTC (F6:
    // 16:00 US/Eastern gives 20:00 UTC).
    for (id, gtd) in [(10, "20260930 16:00:00 US/Eastern"), (11, "20260930 20:00:00 UTC"), (12, "20260930-20:00:00")] {
        client.place_order(id, &spy(), &order(gtd)).unwrap();
        match rx.try_recv().unwrap() {
            ControlCommand::Order(OrderRequest::SubmitLimitEx { attrs, .. }) => {
                assert_eq!(crate::config::unix_to_ib_utc_dash(attrs.good_till), "20260930-20:00:00", "{gtd}")
            }
            other => panic!("expected SubmitLimitEx, got {:?}", other),
        }
    }
}

#[test]
fn place_order_trailing_stop_limit_without_stop_price_is_refused() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL LIMIT".into(),
        lmt_price_offset: 0.5, aux_price: 2.0, ..Default::default()
    };
    client.place_order(3, &spy(), &order).unwrap();
    assert!(rx.try_iter().all(|c| !matches!(c, ControlCommand::Order(_))), "nothing sent");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "error:3:321:Error validating request.-'bH' : cause - Please enter a stop price"), "{:?}", w.events);
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
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitLimitEx {
        attrs: crate::types::OrderAttrs { algo: Some(crate::types::OrderAlgo::Params(crate::types::AlgoParams::Vwap { .. })), .. }, .. })), "{cmd:?}");
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
        ControlCommand::Order(OrderRequest::SubmitLimitEx { tif, attrs, .. }) => {
            assert_eq!(tif, b'1');
            assert_eq!(attrs.parent_id, 100);
            assert_eq!(attrs.oca_group_str, "BR1");
            assert!(matches!(attrs.algo, Some(crate::types::OrderAlgo::Params(crate::types::AlgoParams::Twap { .. }))));
        }
        cmd => panic!("expected a limit order with its algo, got {:?}", cmd),
    }
    let adaptive = Order { algo_strategy: "Adaptive".into(), algo_params: vec![], ..order.clone() };
    client.place_order(102, &spy(), &adaptive).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitLimitEx { tif, attrs, .. }) => {
            assert_eq!(tif, b'1');
            assert_eq!(attrs.parent_id, 100);
            assert!(matches!(attrs.algo, Some(crate::types::OrderAlgo::Adaptive(crate::types::AdaptivePriority::Normal))));
        }
        cmd => panic!("expected a limit order with its algo, got {:?}", cmd),
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

// ibx#462, captured 02/10/2026 (b1_462_whatif): a what-if sent with the id
// of a live order the server has not answered yet is refused with 103 and
// nothing is sent; the live order goes on.
#[test]
fn what_if_on_an_unanswered_order_id_is_a_duplicate() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let live = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 165.16, ..Default::default()
    };
    client.place_order(79, &spy(), &live).unwrap();
    while rx.try_recv().is_ok() {}
    client.place_order(79, &spy(), &Order { lmt_price: 168.46, what_if: true, ..live.clone() }).unwrap();
    assert!(rx.try_recv().is_err(), "nothing is sent");
    let errors = shared.orders.drain_order_errors();
    assert_eq!(errors.iter().map(|e| (e.0, e.1, e.2.as_str())).collect::<Vec<_>>(), [(79, 103, "Duplicate order id")]);
    assert_eq!(client.core.tracked_order(79).map(|o| o.what_if), Some(false));
    assert!(client.core.what_if_orders.lock().unwrap().is_empty());
}

// ibx#462 (`trader.order.bQ.a(gi, e3, fq)@263-321`; paper 02/10/2026 phase
// 72): an order-message reply comes before the data reply. Each gives an
// open_order, the first with no margins; the preview is kept until the
// data reply, which carries the margins.
#[test]
fn what_if_order_message_reply_then_data_reply() {
    let (client, _rx, shared) = test_client();
    client.core.track_what_if(72, spy(), Order { what_if: true, ..Default::default() });
    let mut message = what_if_reply(72, [0.0; 3], [0.0; 3], 0.0);
    message.state = WhatIfState { status: "PreSubmitted".into(), commission: Some(0.0), warning_text: "Warning".into(), ..Default::default() };
    message.final_reply = false;
    shared.orders.push_what_if(message);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events.iter().filter(|e| e.starts_with("open_order:72:PreSubmitted:initB=:")).count(), 1, "{:?}", w.events);
    assert!(client.core.peek_what_if(72).is_some(), "the preview still waits");

    shared.orders.push_what_if(what_if_reply(72, [4943.4, 4125.35, 954397.0], [12855.55, 11566.05, 954397.0], 1.0003));
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("open_order:72:PreSubmitted:initB=4943.4:initC=7912.15:")), "{:?}", w.events);
    assert!(client.core.peek_what_if(72).is_none());
}

// ibx#462, captured 02/10/2026: a what-if with transmit off is refused with
// the reference's 321 and nothing is sent.
#[test]
fn what_if_with_transmit_off_is_refused() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let preview = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(), lmt_price: 297.29,
        what_if: true, transmit: false, ..Default::default()
    };
    client.place_order(76, &spy(), &preview).unwrap();
    assert!(rx.try_recv().is_err(), "nothing is sent");
    let errors = shared.orders.drain_order_errors();
    assert_eq!(errors.iter().map(|e| (e.0, e.1, e.2.as_str())).collect::<Vec<_>>(),
        [(76, 321, "Error validating request.-'v' : cause - What-If order should have transmit flag set to TRUE.")]);
}

// ibx#462, captured 02/10/2026: a refused what-if (a data reply with a
// reason) gives open_order with its values, then error 201.
#[test]
fn refused_what_if_gives_open_order_then_201() {
    let (client, _rx, shared) = test_client();
    let mut reply = what_if_reply(77, [4943.27, 4125.25, 954395.81], [1093819889.17, 994381596.62, 823189.11], 0.0);
    reply.state.commission = None;
    reply.state.reject_reason = "YOUR ORDER IS NOT ACCEPTED.".into();
    shared.orders.push_what_if(reply);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let open = w.events.iter().position(|e| e.starts_with("open_order:77:PreSubmitted")).expect("open_order");
    assert!(w.events[open].contains("initC=1093814945.9:"), "{}", w.events[open]);
    assert!(w.events[open].contains("eqlC=-131206.70000000007:"), "{}", w.events[open]);
    assert!(w.events[open].ends_with(&format!("comm={}", f64::MAX)), "{}", w.events[open]);
    let error = w.events.iter().position(|e| e.starts_with("error:77:201")).expect("error 201");
    assert!(open < error);
}

// ibx#462: a what-if with the id of a working order is a preview, never a
// modify; the working order stays tracked as placed. The preview is kept
// apart and answered with open_order only.
#[test]
fn what_if_on_a_working_order_id_is_a_preview_not_a_modify() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let working = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(), lmt_price: 150.0, ..Default::default()
    };
    client.place_order(73, &spy(), &working).unwrap();
    while rx.try_recv().is_ok() {}
    // The server answered the working order.
    client.core.update_order_status(73, "PreSubmitted", 0.0, 100.0);

    let preview = Order { lmt_price: 149.0, what_if: true, ..working.clone() };
    client.place_order(73, &spy(), &preview).unwrap();
    let cmds: Vec<ControlCommand> = rx.try_iter().collect();
    assert!(cmds.iter().any(|c| matches!(c, ControlCommand::Order(OrderRequest::SubmitWhatIf { .. }))), "{cmds:?}");
    assert!(!cmds.iter().any(|c| matches!(c, ControlCommand::Order(OrderRequest::Modify { .. }))));
    assert_eq!(client.core.tracked_order(73).map(|o| (o.lmt_price, o.what_if)), Some((150.0, false)));

    shared.orders.push_what_if(what_if_reply(73, [0.0; 3], [0.0; 3], 0.0));
    let mut w = crate::api::wrapper::tests::RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("open_order:73:PreSubmitted")), "{:?}", w.events);
    assert!(!w.events.iter().any(|e| e.starts_with("order_status:73")), "{:?}", w.events);
    assert!(client.core.tracked_order(73).is_some(), "the working order is still tracked");
    assert!(client.core.what_if_orders.lock().unwrap().is_empty());
}

// ibx#462: an algo or Adaptive what-if is a preview of that order, not a
// real algo order.
#[test]
fn algo_what_if_is_a_preview() {
    for algo in ["Adaptive", "Twap"] {
        let (client, rx, shared) = test_client();
        shared.market.set_instrument_count(1);
        let order = Order {
            action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 150.0,
            algo_strategy: algo.into(), what_if: true, ..Default::default()
        };
        client.place_order(74, &spy(), &order).unwrap();
        let cmd = rx.try_recv().unwrap();
        assert!(matches!(&cmd, ControlCommand::Order(OrderRequest::SubmitWhatIf { request })
            if matches!(&**request, OrderRequest::SubmitLimitEx { attrs, .. } if attrs.algo.is_some())), "{algo}: {cmd:?}");
        assert!(client.core.tracked_order(74).is_none(), "{algo}: not an open order");
    }
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
    let stk = Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), ..Default::default() };
    let order = Order { action: "BUY".into(), total_quantity: 100.0, order_type: "MKT".into(), ..Default::default() };
    client.place_order(1, &stk, &order).unwrap();
    assert!(rx.try_recv().is_ok());
}

// ibx#485: an action the reference does not know is its 321 refusal, an
// error callback; nothing goes to the engine.
#[test]
fn place_order_invalid_action_is_refused_with_321() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "INVALID".into(), total_quantity: 100.0, order_type: "MKT".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();
    assert!(rx.try_recv().is_err(), "nothing sent");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.contains(&"error:1:321:Error validating request.-'bH' : cause - Invalid side field was entered".to_string()),
        "{:?}", w.events);
}

// ibx#485: order id 0 is refused with 10149, as the reference
// (`jextend.bH.W()@29-55`); ibx used to take the next id.
#[test]
fn place_order_with_id_zero_is_refused_with_10149() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MKT".into(), ..Default::default()
    };
    client.place_order(0, &spy(), &order).unwrap();
    assert!(rx.try_recv().is_err(), "nothing sent");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.contains(&"error:0:10149:Invalid order id: 0".to_string()), "{:?}", w.events);
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

// The global cancel is one request for the whole book, whatever the
// contracts the session registered: the engine cancels every order it
// holds, those of earlier sessions too, as the reference.
#[test]
fn req_global_cancel_sends_one_global_cancel() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(2);
    client.req_global_cancel().unwrap();
    let cmds: Vec<ControlCommand> = rx.try_iter().collect();
    assert_eq!(cmds.len(), 1);
    assert!(matches!(cmds[0], ControlCommand::Order(OrderRequest::GlobalCancel)));
}

#[test]
fn req_global_cancel_without_contracts_still_goes() {
    let (client, rx, _shared) = test_client();
    client.req_global_cancel().unwrap();
    assert!(matches!(rx.try_recv(), Ok(ControlCommand::Order(OrderRequest::GlobalCancel))));
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
        aux_price: 0.0,
        ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("aux_price"));
}

// The auxPrice left unset (the official API's default): the reference's
// refusal of a stop type without its stop price, of MIT and LIT without
// their trigger price; nothing is sent.
#[test]
fn stop_types_with_unset_aux_price_get_the_reference_refusal() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let cases = [
        ("STP", "Please enter a stop price"), ("STP LMT", "Please enter a stop price"),
        ("STP PRT", "Please enter a stop price"), ("MIT", "Invalid Trigger Price"), ("LIT", "Invalid Trigger Price"),
    ];
    for (i, (order_type, cause)) in cases.iter().enumerate() {
        let order = Order {
            action: "SELL".into(), total_quantity: 100.0, order_type: (*order_type).into(),
            lmt_price: 145.0, ..Default::default()
        };
        assert_eq!(order.aux_price, f64::MAX);
        client.place_order(i as i64 + 1, &spy(), &order).unwrap();
        assert!(rx.try_iter().all(|c| !matches!(c, ControlCommand::Order(_))), "{order_type}: nothing sent");
        let mut w = RecordingWrapper::default();
        client.process_msgs(&mut w);
        let want = format!("error:{}:321:Error validating request.-'bH' : cause - {}", i + 1, cause);
        assert!(w.events.contains(&want), "{order_type}: {:?}", w.events);
    }
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
        lmt_price: 144.0, aux_price: 0.0, ..Default::default()
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
        aux_price: 0.0, ..Default::default()
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
        aux_price: 0.0, ..Default::default()
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
        lmt_price: 150.0, aux_price: 0.0, ..Default::default()
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
fn req_historical_data_refuses_unknown_bar_size_with_321() {
    let (client, rx, shared) = test_client();
    // ibx#430: "1 sec" is not a reference size; the refusal is error 321.
    client.req_historical_data(5, &spy(), "", "2 D", "1 sec", "TRADES", true, 1, false).unwrap();
    assert!(rx.try_recv().is_err(), "nothing may reach the engine");
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!((errors[0].0, errors[0].1), (5, 321));
    assert!(errors[0].2.contains("bar size setting is invalid"), "got: {}", errors[0].2);
}

#[test]
fn req_historical_data_accepts_reference_bar_size_in_any_case() {
    let (client, rx, shared) = test_client();
    // "1 Min" is "1 min" for the reference (ibx#430), never 5-minute bars (ibx#232).
    client.req_historical_data(5, &spy(), "", "2 D", "1 Min", "TRADES", true, 1, false).unwrap();
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::FetchHistorical { .. }));
    assert!(shared.reference.drain_historical_errors().is_empty());
}

#[test]
fn req_historical_data_refuses_unknown_what_to_show_and_format_date() {
    let (client, rx, shared) = test_client();
    client.req_historical_data(5, &spy(), "", "2 D", "1 min", "TRADE", true, 1, false).unwrap();
    client.req_historical_data(6, &spy(), "", "2 D", "1 min", "TRADES", true, 5, false).unwrap();
    client.req_historical_data(7, &spy(), "2026-01-02", "2 D", "1 min", "TRADES", true, 1, false).unwrap();
    assert!(rx.try_recv().is_err());
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors[0], (5, 321, "Error validating request.-'bM' : cause - What to show value of TRADE rejected.".to_string()));
    assert_eq!(errors[1], (6, 321, "Error validating request.-'bM' : cause - Date formatting selection of 5 rejected.".to_string()));
    assert_eq!((errors[2].0, errors[2].1), (7, 10314));
}

// ── ibx#427: a contract without conId is looked up first ──

#[test]
fn historical_requests_without_con_id_ask_for_the_contract_first() {
    let (client, rx, _shared) = test_client();
    let aapl = Contract { symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    client.req_historical_data(1, &aapl, "", "1 D", "1 hour", "TRADES", true, 1, false).unwrap();
    client.req_head_time_stamp(2, &aapl, "TRADES", true, 1).unwrap();
    client.req_histogram_data(3, &aapl, true, "1 week").unwrap();
    client.req_historical_ticks(4, &aapl, "", "20260102 10:00:00", 10, "TRADES", true, false, &[]).unwrap();
    client.req_historical_schedule(5, &aapl, "", "1 M", true).unwrap();
    client.req_fundamental_data(6, &aapl, "ReportSnapshot").unwrap();
    for expected in 1..=6i64 {
        match rx.try_recv().unwrap() {
            ControlCommand::ResolveContract { req_id, lookup, request } => {
                assert_eq!(req_id, expected);
                assert_eq!((lookup.symbol.as_str(), lookup.sec_type.as_str(), lookup.currency.as_str()), ("AAPL", "STK", "USD"));
                assert!(!matches!(*request, ControlCommand::ResolveContract { .. }));
            }
            other => panic!("expected ResolveContract, got {:?}", other),
        }
    }
}

#[test]
fn req_historical_data_sends_include_expired() {
    let (client, rx, _shared) = test_client();
    let fut = Contract { con_id: 495512551, sec_type: "FUT".into(), exchange: "CME".into(), include_expired: true, ..Default::default() };
    client.req_historical_data(1, &fut, "", "1 D", "1 hour", "TRADES", true, 1, false).unwrap();
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::FetchHistorical { con_id: 495512551, include_expired: true, .. }));
}

#[test]
fn req_historical_data_schedule_asks_for_the_trading_schedule() {
    let (client, rx, shared) = test_client();
    client.req_historical_data(8, &spy(), "", "1 M", "1 day", "SCHEDULE", true, 1, false).unwrap();
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::FetchHistoricalSchedule { req_id: 8, .. }));
    client.req_historical_data(9, &spy(), "", "1 M", "1 hour", "SCHEDULE", true, 1, false).unwrap();
    assert!(rx.try_recv().is_err());
    assert_eq!(shared.reference.drain_historical_errors()[0].2,
        "Error validating request.-'bM' : cause - Only daily resolution supported for Schedule requests");
}

#[test]
fn req_historical_data_streams_every_bar_size_and_refuses_as_the_reference() {
    let (client, rx, shared) = test_client();
    // ibx#429: every legal bar size streams, as the reference.
    for size in ["1 min", "30 secs", "2 hours"] {
        client.req_historical_data(5, &spy(), "", "1 D", size, "TRADES", true, 1, true).unwrap();
        assert!(matches!(rx.try_recv().unwrap(), ControlCommand::FetchHistorical { keep_up_to_date: true, format_date: 1, .. }));
    }
    client.req_historical_data(6, &spy(), "20261001 10:00:00 US/Eastern", "1 D", "1 hour", "TRADES", false, 1, true).unwrap();
    client.req_historical_data(7, &spy(), "", "1 D", "1 hour", "BID_ASK", false, 1, true).unwrap();
    client.req_historical_data(8, &spy(), "", "1 D", "1 hour", "ADJUSTED_LAST", false, 1, true).unwrap();
    assert!(rx.try_recv().is_err(), "no query");
    let errors: Vec<(i64, i32, String)> = shared.reference.drain_historical_errors();
    assert_eq!(errors, vec![
        (6, 321, "Error validating request.-'bM' : cause - End date not supported with live updates".to_string()),
        (7, 321, "Error validating request.-'bM' : cause - Source price not supported with live updates".to_string()),
        (8, 321, "Error validating request.-'bM' : cause - Source price not supported with live updates".to_string()),
    ]);
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
        ControlCommand::FetchHeadTimestamp { req_id, con_id, what_to_show, use_rth, .. } => {
            assert_eq!(req_id, 10);
            assert_eq!(con_id, 756733);
            assert_eq!(what_to_show, "TRADES");
            assert!(use_rth);
        }
        _ => panic!("expected FetchHeadTimestamp"),
    }
}

// ── ibx#305: the contract's secType and exchange reach the engine ──

#[test]
fn historical_requests_carry_contract_sec_type_and_exchange() {
    let (client, rx, _shared) = test_client();
    let fut = Contract {
        con_id: 815824267, symbol: "MNQ".into(), sec_type: "FUT".into(),
        exchange: "CME".into(), ..Default::default()
    };
    client.req_historical_data(1, &fut, "", "1 D", "1 hour", "TRADES", false, 1, false).unwrap();
    client.req_head_time_stamp(2, &fut, "TRADES", false, 1).unwrap();
    client.req_historical_ticks(3, &fut, "", "20260928 20:00:00", 100, "TRADES", false, false, &[]).unwrap();
    client.req_historical_schedule(4, &fut, "", "1 D", true).unwrap();
    client.req_histogram_data(5, &fut, false, "1 week").unwrap();
    client.req_real_time_bars(6, &fut, 5, "TRADES", false).unwrap();
    for _ in 0..6 {
        let (st, ex) = match rx.try_recv().unwrap() {
            ControlCommand::FetchHistorical { sec_type, exchange, .. }
            | ControlCommand::FetchHeadTimestamp { sec_type, exchange, .. }
            | ControlCommand::FetchHistoricalTicks { sec_type, exchange, .. }
            | ControlCommand::FetchHistoricalSchedule { sec_type, exchange, .. }
            | ControlCommand::FetchHistogramData { sec_type, exchange, .. }
            | ControlCommand::SubscribeRealTimeBar { sec_type, exchange, .. } => (sec_type, exchange),
            other => panic!("unexpected command {:?}", other),
        };
        assert_eq!((st.as_str(), ex.as_str()), ("FUT", "CME"));
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

// ibx#440: a checked request goes to the engine with the type as the
// reference reads it; a refused one gets 321 and nothing is sent.
#[test]
fn req_sec_def_opt_params_checks_then_sends() {
    let (client, rx, _shared) = test_client();
    client.req_sec_def_opt_params(1, "AAPL", "", "cs", 265598).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::FetchSecDefOptParams { req_id, underlying_symbol, fut_fop_exchange, underlying_sec_type, underlying_con_id } => {
            assert_eq!((req_id, underlying_symbol.as_str(), fut_fop_exchange.as_str(), underlying_sec_type.as_str(), underlying_con_id),
                (1, "AAPL", "", "STK", 265598));
        }
        cmd => panic!("expected FetchSecDefOptParams, got {:?}", cmd),
    }
    client.req_sec_def_opt_params(2, "AAPL", "", "OPT", 265598).unwrap();
    client.req_sec_def_opt_params(3, "ES", "", "FUT", 495512563).unwrap();
    client.req_sec_def_opt_params(4, "AAPL", "", "STK", -1).unwrap();
    assert!(rx.try_recv().is_err(), "nothing is sent for a refused request");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let errors: Vec<String> = w.events.iter().filter(|e| e.starts_with("error:")).cloned().collect();
    assert_eq!(errors, [
        "error:2:321:Error validating request.-'cp' : cause - Invalid security type - OPT",
        "error:3:321:Error validating request.-'cp' : cause - Missing exchange for security type FUT",
        "error:4:321:Error validating request.-'cp' : cause - Invalid contract id",
    ]);
}

// ibx#440: one callback per row, then the end; an empty answer gives the
// end only.
#[test]
fn option_chain_rows_then_end() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_option_chains(5, vec![crate::control::optparams::OptionChain {
        exchange: "SMART".into(), underlying_con_id: 265598, trading_class: "AAPL".into(), multiplier: "100".into(),
        expirations: vec!["20261016".into(), "20261120".into()], strikes: vec![5.0, 297.5],
    }]);
    shared.reference.push_option_chains(6, vec![]);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, [
        "sec_def_opt_param:5:SMART:265598:AAPL:100:20261016,20261120:[5.0, 297.5]",
        "sec_def_opt_param_end:5",
        "sec_def_opt_param_end:6",
    ]);
}

// ibx#439: the pattern is checked and trimmed as the reference.
#[test]
fn req_matching_symbols_checks_and_trims_the_pattern() {
    let (client, rx, _shared) = test_client();
    client.req_matching_symbols(9, "  BRK   A ").unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::FetchMatchingSymbols { req_id, pattern } => assert_eq!((req_id, pattern.as_str()), (9, "BRK A")),
        _ => panic!("expected FetchMatchingSymbols"),
    }
    for bad in ["", "   ", "\t"] {
        client.req_matching_symbols(2, bad).unwrap();
    }
    client.req_matching_symbols(3, "MS\tFT").unwrap();
    client.req_matching_symbols(4, "Soci\u{e9}t\u{e9}").unwrap();
    assert!(rx.try_recv().is_err(), "nothing is sent for a refused pattern");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let errors: Vec<String> = w.events.iter().filter(|e| e.starts_with("error:")).cloned().collect();
    let empty = "error:2:321:Error validating request.-'ce' : cause - Pattern must not be empty";
    assert_eq!(errors, [
        empty.to_string(), empty.to_string(), empty.to_string(),
        "error:3:321:Error validating request.-'ce' : cause - Invalid pattern: 'MS\tFT'".to_string(),
        "error:4:321:Error validating request.-'ce' : cause - Invalid pattern: 'Soci\u{e9}t\u{e9}'".to_string(),
    ]);
}

// ibx#439: a row reaches the client with its conId, API security type,
// description and issuer id.
#[test]
fn process_msgs_symbol_samples_carry_the_row_fields() {
    #[derive(Default)]
    struct Samples(Vec<crate::api::types::ContractDescription>);
    impl Wrapper for Samples {
        fn symbol_samples(&mut self, _req_id: i64, descriptions: &[crate::api::types::ContractDescription]) {
            self.0.extend_from_slice(descriptions);
        }
    }
    let (client, _rx, shared) = test_client();
    shared.reference.push_matching_symbols(8, vec![SymbolMatch {
        con_id: -1, symbol: "".into(), sec_type: "BOND".into(), currency: "USD".into(),
        primary_exchange: "".into(), description: "MICROSOFT CORP".into(), issuer_id: "e1393444".into(),
        derivative_types: vec![],
    }]);
    let mut w = Samples::default();
    client.process_msgs(&mut w);
    assert_eq!(w.0.len(), 1);
    let d = &w.0[0];
    assert_eq!((d.con_id, d.sec_type.as_str(), d.description.as_str(), d.issuer_id.as_str()),
        (-1, "BOND", "MICROSOFT CORP", "e1393444"));
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
    let sub = crate::api::types::ScannerSubscription {
        instrument: "STK".into(), location_code: "STK.US.MAJOR".into(),
        scan_code: "TOP_PERC_GAIN".into(), number_of_rows: 25, ..Default::default()
    };
    client.req_scanner_subscription(3, &sub, &[], &[]).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::SubscribeScanner { req_id, subscription, .. } => {
            assert_eq!(req_id, 3);
            assert_eq!(subscription.scan_code, "TOP_PERC_GAIN");
            assert_eq!(subscription.number_of_rows, 25);
            assert!(subscription.filters.is_empty(), "unset fields give no filter");
        }
        _ => panic!("expected SubscribeScanner"),
    }
}

// ibx#456: the set fields become filters with the reference codes, order
// and number text; a filter option replaces a field and moves to the end.
#[test]
fn req_scanner_subscription_filters() {
    let (client, rx, _shared) = test_client();
    let sub = crate::api::types::ScannerSubscription {
        instrument: "STK".into(), location_code: "STK.US.MAJOR".into(), scan_code: "TOP_PERC_GAIN".into(),
        above_price: 10.0, below_price: -1.0, above_volume: 1_000_000, market_cap_above: 1e7,
        moody_rating_above: "A".into(), coupon_rate_below: 5.5, exclude_convertible: true,
        average_option_volume_above: i32::MAX, stock_type_filter: "Stock".into(),
        ..Default::default()
    };
    let opts = [TagValue { tag: "volumeAbove".into(), value: "500".into() },
                TagValue { tag: "usdPriceAbove".into(), value: "2".into() }];
    client.req_scanner_subscription(1, &sub, &[], &opts).unwrap();
    let Ok(ControlCommand::SubscribeScanner { subscription, .. }) = rx.try_recv() else { panic!("expected SubscribeScanner") };
    let got: Vec<String> = subscription.filters.iter().map(|(c, v)| format!("{c}={v}")).collect();
    assert_eq!(got, ["priceAbove=10.0", "marketCapAbove1e6=1.0E7", "moodyRatingAbove=A", "couponRateBelow=5.5",
                     "excludeConvertible=true", "stkTypes=exc:ETF", "volumeAbove=500", "usdPriceAbove=2"]);

    for (t, want) in [("ETF", "inc:ETF"), ("reit", "inc:REIT"), ("ALL", ""), ("junk", "")] {
        let s = crate::api::types::ScannerSubscription { stock_type_filter: t.into(), ..Default::default() };
        let r = crate::client_core::ClientCore::scanner_request(&s, &[], &[]).unwrap();
        let v = r.filters.iter().find(|(c, _)| c == "stkTypes").map(|(_, v)| v.as_str()).unwrap_or("");
        assert_eq!(v, want, "{t}");
    }
}

// ibx#456: refusals of the reference, in its order.
#[test]
fn req_scanner_subscription_refusals() {
    let (client, rx, _shared) = test_client();
    let sub = crate::api::types::ScannerSubscription::default();
    let tv = |t: &str, v: &str| TagValue { tag: t.into(), value: v.into() };
    client.req_scanner_subscription(1, &sub, &[], &[tv("priceAbove", "")]).unwrap();
    client.req_scanner_subscription(2, &sub, &[tv("foo", "1")], &[]).unwrap();
    client.req_scanner_subscription(3, &sub, &[tv("manual", "2")], &[]).unwrap();
    client.req_scanner_subscription(4, &sub, &[tv("manual", "1")], &[]).unwrap();
    assert!(rx.try_recv().is_err(), "nothing sent");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    for want in [
        "error:1:320:Error reading request:Not a key-value pair in generic options list: priceAbove=",
        "error:2:10337:Misc options key=foo is invalid in ReqScannerSubscription(22) request. Valid keys are: manual",
        "error:3:10338:Misc options value=2 is invalid for key=manual in ReqScannerSubscription(22) request. Valid values are: 0, 1",
        "error:4:321:Error validating request.-'co' : cause - Historical data: 'manual' requires Verified API.",
    ] {
        assert!(w.events.iter().any(|e| e == want), "{want} not in {:?}", w.events);
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
    let (client, rx, shared) = test_client();
    shared.reference.set_news_sources(vec!["BRFG".into()]);
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
    let (client, rx, shared) = test_client();
    shared.reference.set_news_sources(vec!["BRFG".into()]);
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

// ibx#459: the reference's local checks of news requests (321), and the
// 300 cap.
#[test]
fn news_request_refusals() {
    let (client, rx, shared) = test_client();
    shared.reference.set_news_sources(vec!["BRFG".into(), "DJ-N".into()]);
    client.req_historical_news(1, 0, "BRFG+BZ", "", "", 5).unwrap();
    client.req_historical_news(2, 0, "", "", "", 5).unwrap();
    client.req_historical_news(3, 0, "brfg+DJ-N", "", "", 0).unwrap();
    client.req_news_article(4, "FLY", "X").unwrap();
    client.req_news_article(5, "BRFG", "").unwrap();
    assert!(rx.try_recv().is_err(), "nothing sent");
    client.req_historical_news(6, 0, "BRFG+", "", "", 1000).unwrap();
    let Ok(ControlCommand::FetchHistoricalNews { max_results, .. }) = rx.try_recv() else { panic!("expected a request") };
    assert_eq!(max_results, 300);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    for want in [
        "error:1:321:Error validating request.-'ca' : cause - Not subscribed for 'BZ' provider",
        "error:2:321:Error validating request.-'ca' : cause - Not subscribed for '' provider",
        "error:3:321:Error validating request.-'ca' : cause - Total results must be > 0",
        "error:4:321:Error validating request.-'cg' : cause - Not subscribed for 'FLY' provider",
        "error:5:321:Error validating request.-'cg' : cause - Article ID must not be empty",
    ] {
        assert!(w.events.iter().any(|e| e == want), "{want} not in {:?}", w.events);
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Fundamental data
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_fundamental_data_sends_fetch() {
    let (client, rx, shared) = test_client();
    // #434: only a stock may be asked; anything else is 321 at once.
    client.req_fundamental_data(5, &Contract { sec_type: String::new(), ..spy() }, "ReportSnapshot").unwrap();
    assert!(rx.try_recv().is_err());
    assert_eq!(shared.reference.drain_historical_errors(),
        [(5, 321, "Error validating request.-'bL' : cause - Please enter a valid security type".to_string())]);
    let stock = Contract { sec_type: "STK".into(), ..spy() };
    client.req_fundamental_data(6, &stock, "ReportSnapshot").unwrap();
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
    let spy = Contract { exchange: "SMART".into(), ..spy() };
    client.req_historical_ticks(8, &spy, "20260101 09:30:00 US/Eastern", "", 1000, "TRADES", true, false, &[]).unwrap();
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

// ibx#250, ibx#486: a server reject gives the Inactive status, then error
// 201 with the reason, then the status once more, as the reference (every
// 39=8 of the four-leg recordings of 26/09 to 02/10/2026).
#[test]
fn process_msgs_delivers_a_reject_error_after_the_status_and_the_status_again() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_order_update(OrderUpdate {
        order_id: 47, instrument: 0, status: OrderStatus::Rejected,
        filled_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, remaining_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_fill_price: 0,
        perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    shared.orders.push_order_notice(47, 201, "Order rejected - reason:too big".into());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let order: Vec<&str> = w.events.iter().map(|e| e.as_str())
        .filter(|e| e.starts_with("error:47:") || e.starts_with("order_status:47:")).collect();
    assert_eq!(order.len(), 3, "{:?}", w.events);
    assert!(order[0].starts_with("order_status:47:Inactive"), "{order:?}");
    assert_eq!(order[1], "error:47:201:Order rejected - reason:too big");
    assert!(order[2].starts_with("order_status:47:Inactive"), "{order:?}");
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

// A server reject of a cancel or modify gives no error and no status, as
// the reference (ibx#252): the status request it triggers sets the state.
#[test]
fn process_msgs_cancel_reject_type_1_gives_no_callback() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_cancel_reject(CancelReject {
        order_id: 44, instrument: 0, reject_type: 1, reason_code: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);
    assert!(shared.orders.drain_cancel_rejects().is_empty(), "the reject is consumed");
}

#[test]
fn process_msgs_cancel_reject_type_2_gives_no_callback() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_cancel_reject(CancelReject {
        order_id: 44, instrument: 0, reject_type: 2, reason_code: 5, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);
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
    shared.market.push_test_message(0, &q, &Default::default());

    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, vec![1]);

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
    shared.market.push_test_message(0, &q, &Default::default());
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
    shared.market.push_test_message(0, &q, &Default::default());

    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, vec![1]);

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

// ibx#287: a wire size of 1 reached the API as 0.0001, and a US stock's
// sizes missed the round lot. AAPL, lot 40: bid 57 -> 2280, last 2 -> 80,
// volume 1466 as on the wire.
#[test]
fn api_sizes_are_wire_sizes_times_the_round_lot() {
    use crate::engine::market_state::MarketState;
    use crate::protocol::tick_decoder::{self as td, RawTick};
    let (client, _rx, shared) = test_client();
    let mut ms = MarketState::new();
    let id = ms.register(265598);
    ms.set_round_lot(id, 40);
    for (tick_type, magnitude) in [(td::O_BID_SIZE, 57), (td::O_ASK_SIZE, 1), (td::O_LAST_SIZE, 2), (td::O_VOLUME, 1466)] {
        ms.apply_tick(id, 0, false, &RawTick { server_tag: 1, tick_type, magnitude, stats_block: false, first: true });
    }
    shared.market.push_test_message(id, ms.quote(id), &Default::default());
    client.core.req_to_instrument.lock().unwrap().insert(1, id);
    client.core.instrument_to_req.lock().unwrap().insert(id, vec![1]);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    for want in ["tick_size:1:0:2280", "tick_size:1:3:40", "tick_size:1:5:80", "tick_size:1:8:1466"] {
        assert!(w.events.iter().any(|e| e == want), "{want} missing in {:?}", w.events);
    }
}

#[test]
fn process_msgs_multiple_instruments_independent() {
    let (client, _rx, shared) = test_client();
    let mut q0 = Quote::default();
    q0.bid = 150 * PRICE_SCALE;
    shared.market.push_test_message(0, &q0, &Default::default());
    let mut q1 = Quote::default();
    q1.bid = 400 * PRICE_SCALE;
    shared.market.push_test_message(1, &q1, &Default::default());

    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, vec![1]);
    client.core.req_to_instrument.lock().unwrap().insert(2, 1);
    client.core.instrument_to_req.lock().unwrap().insert(1, vec![2]);

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
    client.core.tbt_reqs.lock().unwrap().insert(10, (0, 756733, TbtType::Last));
    shared.market.push_tbt_trade(TbtTrade {
        instrument: 0, req_id: 10, tbt_type: TbtType::Last, price: 150 * PRICE_SCALE, size: 100,
        timestamp: 1700000000, exchange: "ARCA".into(), conditions: "".into(), past_limit: false, unreported: false,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tbt_last:10:1:1700000000:150:100:ARCA")));
}

#[test]
fn process_msgs_dispatches_tbt_quote() {
    let (client, _rx, shared) = test_client();
    client.core.tbt_reqs.lock().unwrap().insert(10, (0, 756733, TbtType::BidAsk));
    shared.market.push_tbt_quote(TbtQuote {
        instrument: 0, req_id: 10, bid: 150 * PRICE_SCALE, ask: 151 * PRICE_SCALE,
        bid_size: 1000, ask_size: 2000, timestamp: 1700000000, bid_past_low: false, ask_past_high: false,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tbt_bidask:10:1700000000:150:151:1000:2000")));
}

#[test]
fn process_msgs_tbt_goes_to_the_request_the_engine_names() {
    let (client, _rx, shared) = test_client();
    // No market data mapping: the tick names its request.
    shared.market.push_tbt_trade(TbtTrade {
        instrument: 5, req_id: 42, tbt_type: TbtType::AllLast, price: 150 * PRICE_SCALE, size: 100,
        timestamp: 0, exchange: "".into(), conditions: "".into(), past_limit: false, unreported: false,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tbt_last:42:2:")), "{:?}", w.events);
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — tick news
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_tick_news() {
    let (client, _rx, shared) = test_client();
    client.core.instrument_to_req.lock().unwrap().insert(0, vec![1]);
    client.core.md_news.lock().unwrap().insert(1, "BRFG,DJ-N".into());
    shared.market.push_tick_news(TickNews {
        instrument: 0,
        provider_code: "BRFG".into(), article_id: "BRFG$123".into(),
        headline: "AAPL beats".into(), timestamp: 1700000000000, extra_data: "A:800015:L:en".into(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "tick_news:1:1700000000000:BRFG:BRFG$123:AAPL beats:A:800015:L:en"), "{:?}", w.events);
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

/// A what-if data reply with these margins (init, maint, equity with
/// loan) before and after, and this commission.
fn what_if_reply(order_id: i64, before: [f64; 3], after: [f64; 3], commission: f64) -> WhatIfResponse {
    WhatIfResponse {
        order_id,
        state: WhatIfState {
            status: "PreSubmitted".into(),
            init_margin_before: Some(before[0]),
            maint_margin_before: Some(before[1]),
            equity_with_loan_before: Some(before[2]),
            init_margin_after: Some(after[0]),
            maint_margin_after: Some(after[1]),
            equity_with_loan_after: Some(after[2]),
            commission: Some(commission),
            ..Default::default()
        },
        final_reply: true,
        ..Default::default()
    }
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — what-if
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_what_if() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_what_if(what_if_reply(42, [0.0; 3], [5000.0, 3000.0, 0.0], 1.0));
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    // open_order only, as the reference answers a preview (ibx#462).
    assert!(w.events.iter().any(|e| e.starts_with("open_order:42:PreSubmitted")));
    assert!(!w.events.iter().any(|e| e.starts_with("order_status:42")));
}

/// Regression: what-if dispatch must populate all 8 OrderState fields in
/// open_order, with no order_status (the reference sends none, ibx#462).
#[test]
fn process_msgs_what_if_emits_full_order_state() {
    let (client, _rx, shared) = test_client();
    // Distinct values per field so any swap/typo is detectable.
    shared.orders.push_what_if(what_if_reply(7, [100.0, 200.0, 300.0], [400.0, 500.0, 600.0], 7.0));
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    let open_idx = w.events.iter().position(|e| e.starts_with("open_order:7:"))
        .expect("open_order callback missing for what-if");
    assert!(!w.events.iter().any(|e| e.starts_with("order_status:7")), "no order_status for a what-if");

    let evt = &w.events[open_idx];
    // status, all 9 margin fields (before/change/after × init/maint/eql), commission.
    assert!(evt.contains(":PreSubmitted:"), "status field missing: {evt}");
    // The double's shortest text, as the reference (ibx#462).
    assert!(evt.contains("initB=100.0:initC=300.0:initA=400.0"), "init margin wrong: {evt}");
    assert!(evt.contains("maintB=200.0:maintC=300.0:maintA=500.0"), "maint margin wrong: {evt}");
    assert!(evt.contains("eqlB=300.0:eqlC=300.0:eqlA=600.0"), "equity-with-loan wrong: {evt}");
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
        ..Default::default()
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
        ..Default::default()
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

// ibx#438: a bond row is a bond contract details message, as the
// reference sends it (captured 02/10/2026: 41 bondContractDetails rows for
// an issuer lookup, then the end).
#[test]
fn process_msgs_dispatches_bond_rows_as_bond_contract_details() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_contract_details(9488, ContractDefinition {
        con_id: 29105555, symbol: "IBM".into(), sec_type: SecurityType::Bond, exchange: "SMART".into(),
        ..Default::default()
    });
    shared.reference.push_contract_details_end(9488);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["bond_contract_details:9488:29105555", "contract_details_end:9488"]);
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — matching symbols
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_symbol_samples() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_matching_symbols(8, vec![
        SymbolMatch {
            con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(),
            currency: "USD".into(), primary_exchange: "NASDAQ".into(),
            description: "Apple Inc".into(), issuer_id: String::new(), derivative_types: vec!["OPT".into()],
        },
    ]);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "symbol_samples:8:1"));
}

// ibx#437: a rule from a definition reply is answered; an unknown id or
// a rule with no price increments gives 322 with request id -1.
#[test]
fn req_market_rule_answers_known_rules_and_refuses_others() {
    use crate::control::contracts::{MarketRule, PriceIncrement};
    let (client, _rx, shared) = test_client();
    shared.reference.push_market_rules(vec![
        MarketRule { rule_id: 109, price_increments: vec![
            PriceIncrement { low_edge: 0.0, increment: 0.01 },
            PriceIncrement { low_edge: 3.0, increment: 0.05 },
        ], ..Default::default() },
        MarketRule { rule_id: 5, price_increments: vec![], ..Default::default() },
    ]);
    let mut w = RecordingWrapper::default();
    client.req_market_rule(109, &mut w);
    client.req_market_rule(999999, &mut w);
    client.req_market_rule(5, &mut w);
    assert_eq!(w.events, vec![
        "market_rule:109:2".to_string(),
        "error:-1:322:Error processing request.-'cd' : cause - Market rule with id = 999999 is missing".to_string(),
        "error:-1:322:Error processing request.-'cd' : cause - Price increment rule for market rule with id = 5 is missing".to_string(),
    ]);
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
        ..Default::default()
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

// #453: reqMktDepthExchanges is answered once per request, from the
// depth routes of the routing table, to the Rust wrapper too.
#[test]
fn process_msgs_dispatches_mkt_depth_exchanges() {
    let shared = std::sync::Arc::new(crate::bridge::SharedState::new());
    let mut engine = crate::engine::hot_loop::HotLoop::new(shared.clone(), None, None);
    engine.set_routing_table(crate::engine::routing::TableKind::MarketData,
        "NASDAQ,STK,Top|Deep2|Deep,-1,*,h,4000,usfarm;MEMX,STK,Top,-1,*,h,4000,usfarm;BEST,STK,AggDeep,1,OTCBB,h,4000,usfarm");
    let (tx, rx) = crossbeam_channel::bounded(4);
    engine.set_control_rx(rx);
    tx.send(ControlCommand::FetchMktDepthExchanges).unwrap();
    engine.poll_once();
    let mut rows: Vec<crate::types::DepthMktDataDescription> = shared.reference.drain_depth_exchanges().unwrap();
    rows.sort_by(|a, b| (a.exchange.clone(), a.service_data_type.clone()).cmp(&(b.exchange.clone(), b.service_data_type.clone())));
    let got: Vec<String> = rows.iter().map(|d| format!("{}/{}/{}/{}/{}", d.exchange, d.sec_type, d.listing_exch, d.service_data_type, d.agg_group)).collect();
    assert_eq!(got, ["NASDAQ/STK//Deep/-1", "NASDAQ/STK//Deep2/-1", "SMART/STK/OTCBB/AggDeep/1"]);

    let (client, _rx, shared) = test_client();
    shared.reference.set_depth_exchanges(rows);
    shared.reference.notify_depth_exchanges();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events.iter().filter(|e| e.starts_with("mkt_depth_exchanges:")).count(), 1, "{:?}", w.events);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("mkt_depth_exchanges:")), "once per request");
}

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
        HistoricalTickMidpoint { time: 1_790_881_200, price: 150.5, size: 0.0 },
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
            time: 1_790_881_200, tick_attrib_last: Default::default(), price: 150.5, size: 100.0,
            exchange: "ARCA".into(), special_conditions: "".into(),
        },
    ]), "TRADES".into(), true);
    shared.reference.push_historical_ticks(11, HistoricalTickData::BidAsk(vec![
        HistoricalTickBidAsk {
            time: 1_790_881_201, tick_attrib_bid_ask: Default::default(), price_bid: 150.4, price_ask: 150.6,
            size_bid: 200.0, size_ask: 300.0,
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
    client.core.instrument_to_req.lock().unwrap().insert(0, vec![42]);
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

// A server reject of a modify gives no error and no status, as the reference
// (ibx#252); a modify of a filled order is refused before anything is sent.
#[test]
fn modify_reject_of_a_filled_order_gives_no_callback() {
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
    assert!(w.events.is_empty(), "a modify reject gives no callback, got: {:?}", w.events);
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

// ibx#463: the reference refuses a modify that changes the side (105),
// the OCA group (10326) or the OCA type (10327) before sending anything
// (from the code read, ORDER-MODIFY.md 5). The order stays as placed.
#[test]
fn modify_of_side_or_oca_is_refused_before_sending() {
    let placed = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(), lmt_price: 150.0,
        oca_group: "G1".into(), oca_type: 2, ..Default::default()
    };
    let cases = [
        (Order { action: "SELL".into(), ..placed.clone() }, 105, "Order being modified does not match original order"),
        (Order { oca_group: "G2".into(), ..placed.clone() }, 10326, "OCA group revision is not allowed"),
        (Order { oca_type: 1, ..placed.clone() }, 10327, "OCA group type revision is not allowed"),
    ];
    for (modified, code, text) in cases {
        let (client, rx, shared) = test_client();
        shared.market.set_instrument_count(1);
        client.place_order(70, &spy(), &placed).unwrap();
        while rx.try_recv().is_ok() {}
        client.place_order(70, &spy(), &modified).unwrap();
        assert!(rx.try_recv().is_err(), "{code}: no replace is sent");
        assert_eq!(shared.orders.drain_order_errors(), [(70, code, text.to_string())]);
        assert_eq!(client.core.tracked_order(70).map(|o| (o.action, o.oca_group, o.oca_type)),
            Some(("BUY".to_string(), "G1".to_string(), 2)), "{code}: the order stays as placed");
    }

    // A group or type given on one side only is not a change; the same
    // values are a plain modify.
    for modified in [
        Order { oca_group: String::new(), oca_type: 0, lmt_price: 149.0, ..placed.clone() },
        Order { lmt_price: 149.0, ..placed.clone() },
    ] {
        let (client, rx, shared) = test_client();
        shared.market.set_instrument_count(1);
        client.place_order(71, &spy(), &placed).unwrap();
        while rx.try_recv().is_ok() {}
        client.place_order(71, &spy(), &modified).unwrap();
        assert!(rx.try_iter().any(|c| matches!(c, ControlCommand::Order(OrderRequest::Modify { order_id: 71, .. }))));
        assert!(shared.orders.drain_order_errors().is_empty());
    }
}

// ibx#467 (captured 28/09/2026): the reference holds an OVERNIGHT or
// OVERNIGHT + DAY order as DAY and a DAY order with includeOvernight as
// OVERNIGHT + DAY, reports that time in force, and refuses with 462 a
// modify that restates another one. A modify with the held one goes out.
#[test]
fn modify_must_restate_the_held_overnight_time_in_force() {
    let lmt = |tif: &str, include_overnight: bool, price: f64| Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: price,
        tif: tif.into(), include_overnight, ..Default::default()
    };
    let cases = [
        ("OVERNIGHT", false, "DAY", "Order modify failed. Cannot change to the new Time in Force.OVERNIGHT"),
        ("OVERNIGHT + DAY", false, "DAY", "Order modify failed. Cannot change to the new Time in Force.OVERNIGHT + DAY"),
        ("DAY", true, "OVERNIGHT + DAY", "Order modify failed. Cannot change to the new Time in Force.DAY"),
    ];
    for (tif, include_overnight, held, text) in cases {
        let (client, rx, shared) = test_client();
        shared.market.set_instrument_count(1);
        client.place_order(72, &spy(), &lmt(tif, include_overnight, 600.0)).unwrap();
        while rx.try_recv().is_ok() {}
        assert_eq!(client.core.tracked_order(72).map(|o| o.tif).as_deref(), Some(held), "{tif}");

        // The placed time in force again: refused, nothing sent.
        client.place_order(72, &spy(), &lmt(tif, include_overnight, 600.1)).unwrap();
        assert!(rx.try_recv().is_err(), "{tif}: no replace");
        assert_eq!(shared.orders.drain_order_errors(), [(72, 462, text.to_string())]);

        // The held one: the replace goes out.
        client.place_order(72, &spy(), &lmt(held, include_overnight, 600.1)).unwrap();
        assert!(rx.try_iter().any(|c| matches!(c, ControlCommand::Order(OrderRequest::Modify { order_id: 72, .. }))), "{tif}");
        assert!(shared.orders.drain_order_errors().is_empty());
    }
}

// The reference checks the side before the order type (from the code
// read: both side checks of a modify run before its order-type check): a
// BUY LMT modified into a BUY STP gets 329 (captured, ib-agent#192 A4b),
// into a SELL STP gets 105. Nothing is sent either way.
#[test]
fn modify_type_change_gets_329_and_a_side_change_105_first() {
    for (action, code) in [("BUY", 329), ("SELL", 105)] {
        let (client, rx, shared) = test_client();
        shared.market.set_instrument_count(1);
        let placed = Order {
            action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 200.0, ..Default::default()
        };
        client.place_order(57, &spy(), &placed).unwrap();
        while rx.try_recv().is_ok() {}
        let stp = Order {
            action: action.into(), total_quantity: 1.0, order_type: "STP".into(), aux_price: 195.0, ..Default::default()
        };
        client.place_order(57, &spy(), &stp).unwrap();
        assert!(rx.try_recv().is_err(), "{action}: nothing sent");
        let errors = shared.orders.drain_order_errors();
        assert_eq!(errors.iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(57, code)], "{action}");
        assert_eq!(client.core.tracked_order(57).map(|o| o.order_type).as_deref(), Some("LMT"));
    }
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

// ibx#285, ibx#466: the reference reads the order id as a 32-bit int; an
// order or a cancel with an id outside that range does not decode there
// and is dropped, with no error.
#[test]
fn an_order_id_outside_the_int_range_drops_the_request() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let lmt = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 1.0, ..Default::default() };
    for id in [1_790_166_425_204, i64::from(i32::MAX) + 1, i64::from(i32::MIN) - 1] {
        client.place_order(id, &spy(), &lmt).unwrap();
        client.cancel_order(id, "").unwrap();
    }
    assert!(rx.try_recv().is_err(), "nothing sent");
    assert!(shared.orders.drain_order_errors().is_empty());
    assert!(client.core.tracked_order(1_790_166_425_204).is_none());
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
    assert!(matches!(kind, OrderKind::TrailPct { trail_percent: 250_000_000, .. }), "{:?}", kind);
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
        ControlCommand::Order(OrderRequest::SubmitTrailingStopPct { trail_percent, .. }) => assert_eq!(trail_percent, 115_000_000),
        other => panic!("expected SubmitTrailingStopPct, got {:?}", other),
    }
    assert!(matches!(ClientCore::order_kind(&order).unwrap(), OrderKind::TrailPct { trail_percent: 115_000_000, .. }));
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
// codes and texts (ib-agent#192 B9b, B10a-e), from the algo definitions
// the server sent.
#[test]
fn bad_algo_parameter_values_are_refused_before_sending() {
    let cases: [(&str, &str, &str, &str); 6] = [
        ("Twap", "strategyType", "Marketable", "443:Order processing failed. Unknown algo attribute:strategyType"),
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
        shared.reference.add_algo_definitions(crate::control::algo::CAPTURED_AE);
        shared.reference.add_algo_definitions(crate::control::algo::CAPTURED_AL_STK);
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

// ibx#263, captured 02/10/2026 (b1_263_algo_refusals): the refusals of the
// open points come back through place_order with their codes and texts and
// nothing is sent, the definitions of that day loaded.
#[test]
fn algo_refusals_of_20261002_through_place_order() {
    type Case = (&'static str, Vec<(&'static str, &'static str)>, f64, &'static str);
    let cases: [Case; 5] = [
        ("Foo", vec![], 165.16, "439:Order processing failed. Algorithm definition not found"),
        ("PctVol", vec![], 165.16,
            "441:Algo attributes validation failed:\n'Target Percentage' is invalid: value is required.\n"),
        ("Vwap", vec![("maxPctVol", "abc")], 165.16, "441:Algo attributes validation failed:maxPctVol=abc"),
        ("Twap", vec![("strategyType", "abc")], 165.16, "443:Order processing failed. Unknown algo attribute:strategyType"),
        ("Adaptive", vec![("adaptivePriority", "Normal")], f64::NAN,
            "110:The price does not conform to the minimum price variation for this contract."),
    ];
    for (i, (strategy, params, price, expected)) in cases.into_iter().enumerate() {
        let (client, rx, shared) = test_client();
        shared.market.set_instrument_count(1);
        shared.reference.add_algo_definitions(include_str!("../../../tests/fixtures/algo/IBALGO-AE-20261002.xml"));
        shared.reference.add_algo_definitions(include_str!("../../../tests/fixtures/algo/IBALGO-AL-STK-20261002.xml"));
        let id = 80 + i as i64;
        let order = Order {
            action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: price,
            algo_strategy: strategy.into(),
            algo_params: params.iter().map(|(t, v)| TagValue { tag: t.to_string(), value: v.to_string() }).collect(),
            ..Default::default()
        };
        client.place_order(id, &spy(), &order).unwrap();
        assert!(rx.try_recv().is_err(), "{strategy}: nothing may be sent");
        let mut w = RecordingWrapper::default();
        client.process_msgs(&mut w);
        let want = format!("error:{}:{}", id, expected);
        assert!(w.events.contains(&want), "{strategy}: {:?}", w.events);
    }
    // A plain LMT with a NaN price too.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: f64::NAN, ..Default::default() };
    client.place_order(85, &spy(), &order).unwrap();
    assert!(rx.try_recv().is_err());
    assert_eq!(shared.orders.drain_order_errors().iter().map(|e| e.1).collect::<Vec<_>>(), [110]);
}

// ibx#416, captured 02/10/2026 (b1_416_time_condition): a time condition
// with a zone goes out in UTC; with no zone it is read in the machine's
// zone after warning 2174; text the reference cannot read is 10314 and
// nothing is sent.
#[test]
fn time_condition_through_place_order() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let with_time = |t: &str| Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 1.0, tif: "GTC".into(),
        outside_rth: true,
        conditions: vec![OrderCondition::Time { time: t.into(), is_more: true }],
        ..Default::default()
    };
    let sent_time = |cmd: ControlCommand| match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { attrs, .. })
        | ControlCommand::Order(OrderRequest::SubmitLimitEx { attrs, .. }) => match attrs.conditions.first() {
            Some(OrderCondition::Time { time, .. }) => time.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    client.place_order(71, &spy(), &with_time("20991231 23:59:59 US/Eastern")).unwrap();
    assert_eq!(sent_time(rx.try_recv().unwrap()), "21000101-04:59:59");
    assert_eq!(client.core.tracked_order(71).map(|o| matches!(&o.conditions[0],
        OrderCondition::Time { time, .. } if time == "20991231 23:59:59 US/Eastern")), Some(true), "tracked as placed");
    while rx.try_recv().is_ok() {}

    client.place_order(72, &spy(), &with_time("20991231 23:59:59")).unwrap();
    let errors = shared.orders.drain_order_errors();
    assert_eq!(errors.iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(72, 2174)]);
    assert_eq!(errors[0].2, crate::client_core::IMPLIED_TIME_ZONE);
    let machine = crate::gateway::machine_time_zone();
    let want = crate::client_core::parse_condition_time("20991231 23:59:59", &machine).unwrap().wire;
    assert_eq!(sent_time(rx.try_recv().unwrap()), want);
    while rx.try_recv().is_ok() {}

    client.place_order(74, &spy(), &with_time("tomorrow")).unwrap();
    assert!(rx.try_recv().is_err(), "nothing is sent");
    assert_eq!(shared.orders.drain_order_errors().iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(74, 10314)]);
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
    assert!(matches!(rx.try_recv(), Ok(ControlCommand::Order(OrderRequest::SubmitLimitEx { attrs, .. })) if attrs.algo.is_some()));
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
    shared.market.push_test_message(0, &q, &Default::default());
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
    shared.market.push_test_message(0, &q, &Default::default());

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:")));

    shared.market.push_test_message(0, &q, &Default::default()); // same quote
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
    shared.market.push_test_message(0, &q, &Default::default());
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
    shared.market.push_test_message(0, &q, &Default::default());

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    q.bid = 99 * PRICE_SCALE;
    shared.market.push_test_message(0, &q, &Default::default());
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

// ibx#399: a lost and restored link reaches the client as errors with id
// -1, and the client stays connected.
#[test]
fn link_notices_are_errors_and_keep_the_client_connected() {
    let (client, _rx, shared) = test_client();
    let mut w = RecordingWrapper::default();

    shared.push_connection_notice(1100, "lost.".into());
    shared.push_connection_notice(2103, "broken:usfarm".into());
    client.process_msgs(&mut w);
    shared.push_connection_notice(1102, "restored.".into());
    client.process_msgs(&mut w);

    assert_eq!(w.events, vec!["error:-1:1100:lost.", "error:-1:2103:broken:usfarm", "error:-1:1102:restored."]);
    assert!(client.is_connected());
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

fn aapl_fill(order_id: OrderId) -> Fill {
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
// frame, after exec_details, with the server's values; the order's
// orderStatus is given again first, as the reference (ibx#486).
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
    assert_eq!(w.events, ["order_status:7:Filled:100:0:336", "commission:0000e0d5.6ab5f36f.01.01:1.0003:USD"]);

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

// ibx#314: an execution of an untracked order has no live callback and is
// returned by req_executions, with its commission report.
#[test]
fn an_untracked_execution_is_returned_by_req_executions_only() {
    let (client, _rx, shared) = test_client();
    let contract = Contract { con_id: 265598, symbol: "AAPL".into(), ..Default::default() };
    let exec = crate::api::types::Execution {
        exec_id: "0000e0d5.6ab5f36f.01.01".into(), side: "BOT".into(), shares: 100.0, order_id: 15,
        ..Default::default()
    };
    let fe = crate::bridge::FillExec { exec_id: "0000e0d5.6ab5f36f.01.01".into(), client_id: 261, ..Default::default() };
    shared.orders.push_untracked_execution(contract, exec, fe);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);

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

// ibx#487: a combo leg's execution shows the leg report's lastLiquidity
// (851=2), not the combo report's (851=1); a filled order no longer tracked
// shows the placing client of the report (captured 30/09/2026,
// i105_combo_fill: openOrder and orderStatus with clientId 198).
#[test]
fn a_fill_takes_the_report_last_liquidity_and_placing_client() {
    let (client, _rx, _shared) = test_client();
    let mut ex = crate::api::types::Execution { last_liquidity: 1, ..Default::default() };
    let fe = crate::bridge::FillExec { last_liquidity: 2, client_id: 198, ..captured_fill_exec() };
    client.core.apply_fill_exec(&mut ex, &fe, 42);
    assert_eq!((ex.last_liquidity, ex.client_id), (2, 198));
    let kept = crate::bridge::FillExec { last_liquidity: 0, ..captured_fill_exec() };
    client.core.apply_fill_exec(&mut ex, &kept, 42);
    assert_eq!(ex.last_liquidity, 2, "a report without 851 keeps the value");

    let mut view = Some(crate::client_core::OrderView {
        contract: Contract::default(), order: crate::api::types::Order::default(),
        state: Default::default(), last_fill_price: 0.0, client_id: 0,
    });
    crate::client_core::ClientCore::report_client(&mut view, &fe);
    let v = view.unwrap();
    assert_eq!((v.client_id, v.order.client_id), (198, 198));
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

// ibx#265: as the reference, every execution first, then the commission
// reports, then the end; no lock is held while the callbacks run, so a
// callback can use the client.
#[test]
fn req_executions_sends_executions_then_reports_with_no_lock_held() {
    let (client, _rx, shared) = test_client();
    for (order_id, exec_id) in [(7, "0000e0d5.6ab5f36f.01.01"), (8, "0000e0d5.6ab5f370.01.01")] {
        shared.orders.push_fill_with_exec(aapl_fill(order_id), crate::bridge::FillExec { exec_id: exec_id.into(), ..Default::default() });
        shared.orders.push_commission_report(crate::api::types::CommissionAndFeesReport {
            exec_id: exec_id.into(), ..captured_report()
        });
    }
    client.process_msgs(&mut RecordingWrapper::default());

    struct Reentrant<'a> { core: &'a ClientCore, events: Vec<String> }
    impl Wrapper for Reentrant<'_> {
        fn exec_details(&mut self, _req_id: i64, _c: &Contract, e: &crate::api::types::Execution) {
            let free = self.core.executions.try_lock().is_ok();
            self.events.push(format!("exec:{}:{}", e.exec_id, free));
        }
        fn commission_and_fees_report(&mut self, r: &crate::api::types::CommissionAndFeesReport) {
            let free = self.core.executions.try_lock().is_ok();
            self.events.push(format!("commission:{}:{}", r.exec_id, free));
        }
        fn exec_details_end(&mut self, req_id: i64) {
            self.events.push(format!("end:{}", req_id));
        }
    }
    let mut w = Reentrant { core: &client.core, events: Vec::new() };
    client.req_executions(1, &crate::api::types::ExecutionFilter::default(), &mut w);
    assert_eq!(w.events, [
        "exec:0000e0d5.6ab5f36f.01.01:true",
        "exec:0000e0d5.6ab5f370.01.01:true",
        "commission:0000e0d5.6ab5f36f.01.01:true",
        "commission:0000e0d5.6ab5f370.01.01:true",
        "end:1",
    ]);
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
        last_liquidity: 0,
        combo: None,
        other_client: false,
        replayed: false,
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

fn update(order_id: OrderId, status: OrderStatus, filled: i64) -> OrderUpdate {
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
    // The account time is in the machine's zone (ibx#487).
    crate::gateway::set_machine_zone_for_test(Some("US/Eastern"));
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
        // In the order of the reference's value map: key, then currency.
        "value:AccountType:INDIVIDUAL:",
        "value:CashBalance:899133.4993:BASE",
        "value:NetLiquidation:953633.06:USD",
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

fn ledger_row(account: &str, currency: &str, cash: f64, nlv: f64) -> crate::bridge::LedgerRow {
    crate::bridge::LedgerRow {
        account: account.into(), currency: currency.into(), real_currency: currency.into(),
        values: vec![(9806, cash), (9819, nlv), (9820, 1.0)],
    }
}

fn summary_event(rows: Vec<crate::bridge::AccountRow>, ledgers: Vec<crate::bridge::LedgerRow>, end: bool) -> crate::bridge::AccountSummaryEvent {
    crate::bridge::AccountSummaryEvent { sr_id: "SR.Socket.1".into(), ledger: !ledgers.is_empty(), end, rows, ledgers }
}

#[test]
fn account_summary_is_a_server_subscription() {
    let (client, rx, shared) = test_client();
    client.req_account_summary(1, "All", "AccountType,NetLiquidation,$LEDGER:USD");
    assert_eq!(summary_sent(&rx), ["sub:SR.Socket.1:AccountType,NetLiquidation,$LEDGER:All"]);

    // Rows as sent; ledger rows of the chosen currency only, written as the
    // reference writes account values; the end. Each frame twice, as the
    // reference (ibx#486: its listener is registered twice).
    shared.portfolio.push_account_summary_event(summary_event(
        vec![summary_row("AccountType", "INDIVIDUAL", ""), summary_row("NetLiquidation", "953633.06", "USD")], vec![], false));
    shared.portfolio.push_account_summary_event(summary_event(vec![],
        vec![ledger_row("DU1", "BASE", 899133.4993, 1.0), ledger_row("DU1", "USD", 899133.4993, 953925.6599)], false));
    shared.portfolio.push_account_summary_event(summary_event(vec![], vec![], true));
    let mut w = SummaryRec::default();
    client.process_msgs(&mut w);
    let usd: Vec<String> = [
        "Currency:USD", "CashBalance:899133.4993", "TotalCashBalance:", "AccruedCash:", "StockMarketValue:",
        "OptionMarketValue:", "FutureOptionValue:", "FuturesPNL:", "NetLiquidationByCurrency:953925.6599",
        "UnrealizedPnL:", "RealizedPnL:", "ExchangeRate:1.00", "FundValue:", "NetDividend:", "MutualFundValue:",
        "MoneyMarketFundValue:", "CorporateBondValue:", "TBondValue:", "TBillValue:", "WarrantValue:",
        "FxCashBalance:", "AccountOrGroup:DU1", "RealCurrency:USD", "IssuerOptionValue:", "Cryptocurrency:",
    ].iter().map(|r| format!("row:1:{r}:USD")).collect();
    let mut want: Vec<String> = Vec::new();
    for _ in 0..2 { want.extend(["row:1:AccountType:INDIVIDUAL:".to_string(), "row:1:NetLiquidation:953633.06:USD".to_string()]); }
    for _ in 0..2 { want.extend(usd.iter().cloned()); }
    for _ in 0..2 { want.push("end:1".to_string()); }
    assert_eq!(w.events, want);

    // A later batch keeps coming, with its own end.
    shared.portfolio.push_account_summary_event(summary_event(vec![summary_row("NetLiquidation", "953642.02", "USD")], vec![], false));
    shared.portfolio.push_account_summary_event(summary_event(vec![], vec![], true));
    let mut w = SummaryRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["row:1:NetLiquidation:953642.02:USD", "row:1:NetLiquidation:953642.02:USD", "end:1", "end:1"]);

    // Cancel: the server subscription is cancelled; later rows are dropped.
    client.cancel_account_summary(1);
    assert_eq!(summary_sent(&rx), ["cancel:SR.Socket.1"]);
    shared.portfolio.push_account_summary_event(summary_event(vec![], vec![], true));
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
        "error:6:322:Error processing request.-'b2' : cause - Maximum number of account summary requests exceeded; \
         desubscribe to previous request first",
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
        "error:9001:322:Error processing request.-'bj' : cause - Duplicate ticker id",
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
    assert_eq!(client.core.instrument_to_req.lock().unwrap().get(&7), Some(&vec![5]));

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
    client.core.instrument_to_req.lock().unwrap().insert(3, vec![9]);
    client.req_pnl(1, "DU123", "");
    client.process_msgs(&mut RecordingWrapper::default());
    let (subs, _) = answer_pnl_quotes(&rx, 3);
    assert!(subs.is_empty());
}

// ibx#461: popup bulletins reach every client; a subscription gets every
// type from then on, and the day's store first only with all_msgs; a
// cancel goes back to popups; a new day empties the store.
#[test]
fn news_bulletin_delivery_modes() {
    let (client, _rx, shared) = test_client();
    let b = |id: i32, t: i32| crate::types::NewsBulletin { msg_id: id, msg_type: t, message: format!("m{id}"), exchange: "X".into() };
    let day = jiff::civil::date(2026, 10, 1);
    let events = |client: &EClient| {
        let mut w = RecordingWrapper::default();
        client.process_msgs(&mut w);
        w.events.into_iter().filter(|e| e.starts_with("news_bulletin:")).collect::<Vec<_>>()
    };
    shared.market.push_news_bulletin_on(b(1, 1), day);
    shared.market.push_news_bulletin_on(b(2, 5), day);
    assert_eq!(events(&client), ["news_bulletin:2:5:m2:X"], "only the popup without a subscription");

    client.req_news_bulletins(false);
    shared.market.push_news_bulletin_on(b(3, 2), day);
    assert_eq!(events(&client), ["news_bulletin:3:2:m3:X"], "no replay without all_msgs");

    client.req_news_bulletins(true);
    assert_eq!(events(&client), ["news_bulletin:1:1:m1:X", "news_bulletin:2:5:m2:X", "news_bulletin:3:2:m3:X"]);

    client.cancel_news_bulletins();
    shared.market.push_news_bulletin_on(b(4, 1), day);
    shared.market.push_news_bulletin_on(b(5, 6), day);
    assert!(!shared.market.push_news_bulletin_on(b(5, 6), day), "a repeated id is dropped");
    assert_eq!(events(&client), ["news_bulletin:5:6:m5:X"]);

    shared.market.push_news_bulletin_on(b(6, 6), jiff::civil::date(2026, 10, 2));
    client.req_news_bulletins(true);
    assert_eq!(events(&client), ["news_bulletin:6:6:m6:X"], "the store holds the new day only");
}

// ibx#426: version-gated client code can read the session level and the
// connection time.
#[test]
fn server_version_and_connection_time() {
    let (client, _rx, _shared) = test_client();
    assert_eq!(client.server_version(), 214);
    let time = client.tws_connection_time();
    assert_eq!(time.split(' ').next().map(str::len), Some(8), "{}", time);
    assert_eq!(time, client.tws_connection_time(), "fixed at connect");
}

// ibx#285: ids are signed, as the reference's: error -1 and a negative
// order id reach the wrapper as they were given, and a cancel of a
// negative order id reaches the engine with that id.
#[test]
fn negative_ids_round_trip() {
    let (client, rx, shared) = test_client();
    client.req_market_data_type(9);
    shared.orders.push_order_error(-7, 201, "Order rejected - reason:test".into());
    shared.orders.push_order_update(OrderUpdate {
        order_id: -7, instrument: 0, status: OrderStatus::Rejected,
        filled_qty_fixed: 0, remaining_qty_fixed: 0, avg_fill_price: 0,
        perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:-1:321:")), "{:?}", w.events);
    assert!(w.events.iter().any(|e| e == "error:-7:201:Order rejected - reason:test"), "{:?}", w.events);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:-7:Inactive")), "{:?}", w.events);

    client.cancel_order(-7, "").unwrap();
    match rx.try_recv() {
        Ok(ControlCommand::Order(OrderRequest::Cancel { order_id })) => assert_eq!(order_id, -7),
        other => panic!("expected the cancel, got {:?}", other),
    }
}

// ibx#285: a negative request id reaches the engine and comes back to the
// wrapper unchanged; it was cast to an unsigned type on the way.
#[test]
fn a_negative_request_id_round_trips_through_the_engine_queues() {
    let (client, rx, shared) = test_client();
    client.cancel_historical_data(-3).unwrap();
    client.cancel_mkt_depth(-4).unwrap();
    match rx.try_recv() {
        Ok(ControlCommand::CancelHistorical { req_id }) => assert_eq!(req_id, -3),
        other => panic!("expected the historical cancel, got {:?}", other),
    }
    match rx.try_recv() {
        Ok(ControlCommand::UnsubscribeDepth { req_id }) => assert_eq!(req_id, -4),
        other => panic!("expected the depth cancel, got {:?}", other),
    }

    shared.reference.push_historical_error(-3, 162, "no data".into());
    shared.reference.push_contract_details_end(-5);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "error:-3:162:no data"), "{:?}", w.events);
    assert!(w.events.iter().any(|e| e == "contract_details_end:-5"), "{:?}", w.events);
}

// ibx#285: the reference reads ids as 32-bit ints; a request with an id
// outside that range does not decode there and is dropped with no error.
// The ends of the range are kept.
#[test]
fn ids_outside_the_reference_range_drop_the_request() {
    let (client, rx, shared) = test_client();
    for id in [i64::from(i32::MIN), i64::from(i32::MAX)] {
        client.cancel_historical_data(id).unwrap();
        match rx.try_recv() {
            Ok(ControlCommand::CancelHistorical { req_id }) => assert_eq!(req_id, id),
            other => panic!("expected the cancel of {id}, got {:?}", other),
        }
    }
    client.cancel_historical_data(i64::from(i32::MAX) + 1).unwrap();
    client.cancel_historical_data(i64::from(i32::MIN) - 1).unwrap();
    let wide = Contract { con_id: 1 << 32, ..spy() };
    client.req_mkt_depth(7, &wide, 5, false).unwrap();
    client.cancel_mkt_data(1 << 40).unwrap();
    assert!(rx.try_recv().is_err(), "nothing sent");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("error:")), "no error: {:?}", w.events);
    assert!(shared.orders.drain_order_errors().is_empty());
}

// ibx#251: an order the engine dropped (filled while the auth link was
// lost) leaves the client with no callback: no open order, no status.
#[test]
fn a_forgotten_order_leaves_without_callbacks() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 10.0, ..Default::default() };
    client.place_order(1, &spy(), &order).unwrap();
    shared.orders.push_forgotten_order(1);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "no callback: {:?}", w.events);
    client.req_open_orders(&mut w);
    assert_eq!(w.events, vec!["open_order_end".to_string()]);
}

// ibx#251: an open-order request made while the auth link is lost gets no
// answer until the order replay of the new logon has ended; then each kind
// is answered once, after the replayed statuses, as in the reference.
#[test]
fn open_order_requests_wait_for_the_order_replay() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 10.0, ..Default::default() };
    client.place_order(1, &spy(), &order).unwrap();

    shared.orders.set_open_orders_held(true);
    let mut w = RecordingWrapper::default();
    client.req_open_orders(&mut w);
    client.req_open_orders(&mut w);
    client.req_all_open_orders(&mut w);
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "no answer during the outage: {:?}", w.events);

    // The replay gives the order's status, then its end lets the requests through.
    shared.orders.push_order_update(OrderUpdate {
        order_id: 1, instrument: 0, status: OrderStatus::Submitted,
        filled_qty_fixed: 0, remaining_qty_fixed: crate::types::QTY_SCALE, avg_fill_price: 0,
        perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    shared.orders.set_open_orders_held(false);
    client.process_msgs(&mut w);
    let kinds: Vec<&str> = w.events.iter().map(|e| {
        if e.starts_with("open_order:1:Submitted") { "open_order" }
        else if e.starts_with("order_status:1:Submitted") { "order_status" }
        else if e == "open_order_end" { "end" }
        else { e.as_str() }
    }).collect();
    // Each listed order with its status, as the reference writes it
    // (OPEN_ORDER then ORDER_STATUS).
    assert_eq!(kinds, vec!["open_order", "order_status", "open_order", "order_status", "end", "open_order", "order_status", "end"]);

    // Answered at once again when the link is up.
    let mut w = RecordingWrapper::default();
    client.req_open_orders(&mut w);
    assert_eq!(w.events.last().map(String::as_str), Some("open_order_end"));
    client.process_msgs(&mut w);
    assert_eq!(w.events.iter().filter(|e| *e == "open_order_end").count(), 1);
}

// reqAllOpenOrders lists every order of the book in its order (a hash
// table keyed by permId, walked bucket by bucket; captured 01/10/2026),
// with the order id and client id the reference shows; reqOpenOrders
// only this client's (`jextend.cu.b(List)@149`).
#[test]
fn open_order_listings_go_in_the_books_order_by_client() {
    use crate::bridge::RichOrderInfo;
    let (client, _rx, shared) = test_client();
    client.core.client_id.store(193, std::sync::atomic::Ordering::Relaxed);
    let replay = [(1790862363895062i64, 0i32), (1790862423941063, 0), (1790863660514062, 193), (1790865742870063, 0)];
    for (seq, (id, owner)) in replay.iter().enumerate() {
        let order = Order { order_id: *id, perm_id: *id, client_id: *owner, total_quantity: 1.0, ..Default::default() };
        let order_state = crate::api::types::OrderState { status: "PreSubmitted".into(), ..Default::default() };
        shared.orders.push_order_info(*id, RichOrderInfo { contract: spy(), order, order_state, last_exec: Default::default() });
        shared.orders.set_api_order_id(*id, 0);
        shared.orders.note_book(*id, seq as u64, seq + 1);
    }
    let all = client.core.open_orders_listing(&shared, crate::client_core::OpenOrdersRequest::All);
    let perms: Vec<i64> = all.iter().map(|(_, t, _)| t.order.perm_id).collect();
    assert_eq!(perms, [1790865742870063, 1790862423941063, 1790863660514062, 1790862363895062]);
    assert!(all.iter().all(|(id, t, _)| *id == 0 && t.order.order_id == 0 && t.remaining == 1.0));
    assert_eq!(all.iter().map(|(.., c)| *c).collect::<Vec<_>>(), [0, 0, 193, 0]);
    let mine = client.core.open_orders_listing(&shared, crate::client_core::OpenOrdersRequest::Open);
    assert_eq!(mine.iter().map(|(_, t, _)| t.order.perm_id).collect::<Vec<_>>(), [1790863660514062]);
}

// reqOpenOrders of a plain TRAIL order placed by symbol (captured
// 05/10/2026, b2_trail): the contract the server reported (its conId) and
// the stop price of its last report (ibx#491).
#[test]
fn open_orders_show_the_reported_contract_and_trail_stop() {
    use crate::bridge::RichOrderInfo;
    let (client, _rx, shared) = test_client();
    let by_symbol = Contract { symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    let placed = Order { order_id: 103, order_type: "TRAIL".into(), aux_price: 1.0, total_quantity: 1.0, ..Default::default() };
    client.core.open_orders.lock().unwrap().insert(103, crate::client_core::TrackedOrder {
        contract: by_symbol, order: placed.clone(), status: "PreSubmitted".into(), filled: 0.0, remaining: 1.0,
        instrument: 0, last_fill_price: 0.0,
    });
    let reported = Order { trail_stop_price: 775.06, ..placed };
    let order_state = crate::api::types::OrderState { status: "PreSubmitted".into(), ..Default::default() };
    shared.orders.push_order_info(103, RichOrderInfo { contract: spy(), order: reported, order_state, last_exec: Default::default() });
    let listed = client.core.open_orders_listing(&shared, crate::client_core::OpenOrdersRequest::Open);
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].1.contract.con_id, listed[0].1.order.trail_stop_price), (756733, 775.06));
}

// An execution of another client's order is kept for reqExecutions, with
// no live callback and no commission report (`jextend.ba.a(dq, aQ)`).
#[test]
fn an_execution_of_another_clients_order_gives_no_callback() {
    let (client, _rx, shared) = test_client();
    let exec = crate::api::types::Execution { exec_id: "0000e0d5.6abe.01.01".into(), order_id: 35, shares: 1.0, ..Default::default() };
    shared.orders.push_untracked_execution(spy(), exec,
        crate::bridge::FillExec { exec_id: "0000e0d5.6abe.01.01".into(), other_client: true, ..Default::default() });
    shared.orders.push_commission_report(crate::api::types::CommissionAndFeesReport {
        exec_id: "0000e0d5.6abe.01.01".into(), commission_and_fees: 1.0, currency: "USD".into(), ..Default::default()
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().all(|e| !e.starts_with("exec") && !e.starts_with("commission")), "{:?}", w.events);
    let mut w = RecordingWrapper::default();
    client.req_executions(4, &Default::default(), &mut w);
    assert!(w.events.iter().any(|e| e.starts_with("exec_details")), "{:?}", w.events);
}

// ═══════════════════════════════════════════════════════════════════
//  Regulatory snapshot (ibx#446)
// ═══════════════════════════════════════════════════════════════════

#[derive(Default)]
struct SnapRec { events: Vec<String> }
impl Wrapper for SnapRec {
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) { self.events.push(format!("error:{req_id}:{code}:{text}")); }
    fn tick_price(&mut self, req_id: i64, tt: i32, p: f64, _: &crate::api::types::TickAttrib) { self.events.push(format!("price:{req_id}:{tt}:{p}")); }
    fn tick_size(&mut self, req_id: i64, tt: i32, s: f64) { self.events.push(format!("size:{req_id}:{tt}:{s}")); }
    fn tick_string(&mut self, req_id: i64, tt: i32, v: &str) { self.events.push(format!("string:{req_id}:{tt}:{v}")); }
    fn tick_snapshot_end(&mut self, req_id: i64) { self.events.push(format!("end:{req_id}")); }
    fn tick_req_params(&mut self, req_id: i64, _: f64, _: &str, _: i64) { self.events.push(format!("params:{req_id}")); }
    fn market_data_type(&mut self, req_id: i64, t: i32) { self.events.push(format!("mdt:{req_id}:{t}")); }
}

/// Answer the snapshot registrations of the engine side with instrument 5.
fn snapshot_engine(rx: crossbeam_channel::Receiver<ControlCommand>) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        while let Ok(cmd) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            match cmd {
                ControlCommand::SubscribeSnapshot { reply_tx: Some(tx), con_id, .. } => {
                    let _ = tx.send(Ok(5));
                    seen.push(format!("snapshot:{con_id}"));
                }
                ControlCommand::DropSnapshot { instrument } => seen.push(format!("drop:{instrument}")),
                ControlCommand::Subscribe { .. } => seen.push("subscribe".into()),
                _ => {}
            }
        }
        seen
    })
}

/// The exchange map of an instrument's BBO exchange (ibx#441).
fn set_exchange_map(shared: &SharedState, instrument: u32, map: Vec<crate::types::SmartComponent>) {
    shared.reference.observe_exchange_map(instrument, "9c", 1);
    shared.reference.set_exchange_map("9c", 1, map);
}

#[test]
fn regulatory_snapshot_delivers_one_batch_then_the_end() {
    let (client, rx, shared) = test_client();
    let engine = snapshot_engine(rx);
    set_exchange_map(&shared, 5, vec![crate::types::SmartComponent {
        bit_number: 0, exchange: "NYSE".into(), exchange_letter: "N".into(),
    }]);
    client.req_mkt_data(1, &spy(), "", false, true).unwrap();
    // The same contract again: 10169; the same request id again: duplicate.
    client.req_mkt_data(2, &spy(), "", false, true).unwrap();
    client.req_mkt_data(1, &spy(), "", false, true).unwrap();
    let mut w = SnapRec::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:2:10169:Regulatory snapshot for SPY is already being fetched in ticker id=1")), "{:?}", w.events);
    assert!(w.events.iter().any(|e| e.starts_with("error:1:322:")), "{:?}", w.events);
    let s = crate::types::PRICE_SCALE;
    let q = crate::types::QTY_SCALE;
    shared.market.push_quote(5, &crate::types::Quote {
        bid: 100 * s, ask: 101 * s, last: 100 * s, bid_size: 2 * q, ask_size: 3 * q, last_size: q,
        volume: 50 * q, high: 102 * s, low: 99 * s, close: 98 * s,
        bid_exch_mask: 1, ask_exch_mask: 1, last_exch_mask: 1, ..Default::default()
    });
    shared.market.push_snapshot_ack(crate::bridge::TickReqParams {
        instrument: 5, min_tick: 0.01, bbo_exchange: "9c".into(), snapshot_permissions: 3,
    });
    let mut w = SnapRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, vec![
        "price:1:2:101", "size:1:3:3", "price:1:1:100", "size:1:0:2", "price:1:4:100", "size:1:5:1",
        "string:1:33:N", "string:1:32:N", "string:1:84:N", "price:1:6:102", "price:1:7:99", "price:1:9:98",
        "size:1:8:50", "end:1",
    ]);
    // Done: a cancel now is for an unknown request.
    client.cancel_mkt_data(1).unwrap();
    let mut w = SnapRec::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:1:300:")));
    let seen = engine.join().unwrap();
    assert_eq!(seen, vec!["snapshot:756733", "drop:5"]);
}

#[test]
fn regulatory_snapshot_without_permission_and_cancel() {
    let (client, rx, shared) = test_client();
    let engine = snapshot_engine(rx);
    client.req_mkt_data(1, &spy(), "", false, true).unwrap();
    shared.market.push_snapshot_ack(crate::bridge::TickReqParams {
        instrument: 5, min_tick: 0.01, bbo_exchange: "9c".into(), snapshot_permissions: 1,
    });
    let mut w = SnapRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, vec!["error:1:10170:No permissions on Regulatory snapshot for SPY"]);
    // A running fetch stops silently on cancel.
    client.req_mkt_data(3, &spy(), "", false, true).unwrap();
    client.cancel_mkt_data(3).unwrap();
    let mut w = SnapRec::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);
    let seen = engine.join().unwrap();
    assert_eq!(seen, vec!["snapshot:756733", "drop:5", "snapshot:756733", "drop:5"]);
}

// ═══════════════════════════════════════════════════════════════════
//  Plain snapshot (ibx#446)
// ═══════════════════════════════════════════════════════════════════

/// Answer the subscriptions of the engine side with instrument 5; returns
/// what was asked: subscribe (with the snapshot flag) and unsubscribe.
fn top_engine(rx: crossbeam_channel::Receiver<ControlCommand>) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        while let Ok(cmd) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            match cmd {
                ControlCommand::Subscribe { reply_tx: Some(tx), con_id, snapshot, .. } => {
                    let _ = tx.send(Ok(5));
                    seen.push(format!("subscribe:{con_id}:{snapshot}"));
                }
                ControlCommand::Unsubscribe { instrument } => seen.push(format!("unsubscribe:{instrument}")),
                _ => {}
            }
        }
        seen
    })
}

fn spy_stk() -> Contract {
    Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), ..Default::default() }
}

/// A farm message for instrument 5 with the given steps (ibx#446).
fn message(shared: &SharedState, q: &crate::types::Quote, steps: &[crate::md_events::MdStep], halted: Option<i64>) {
    let message = crate::md_events::TestMessage { steps: Some(steps), halted, ..Default::default() };
    shared.market.push_test_message(5, q, &message);
}

// Each tick type once, the size with its price; no end on a partial
// batch; the end once bid, ask, last, close and open came; then the
// request is gone and its farm request cancelled.
#[test]
fn plain_snapshot_sends_each_tick_type_once_then_the_end() {
    use crate::md_events::MdStep;
    let (client, rx, shared) = test_client();
    let engine = top_engine(rx);
    set_exchange_map(&shared, 5, vec![crate::types::SmartComponent {
        bit_number: 0, exchange: "NYSE".into(), exchange_letter: "N".into(),
    }]);
    client.req_mkt_data(1, &spy_stk(), "", true, false).unwrap();
    let s = crate::types::PRICE_SCALE;
    let q = crate::types::QTY_SCALE;
    message(&shared, &crate::types::Quote {
        bid: 100 * s, ask: 101 * s, bid_size: 2 * q, ask_size: 3 * q, bid_exch_mask: 1, ..Default::default()
    }, &[MdStep::Quote], None);
    let mut w = SnapRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, vec![
        "mdt:1:1", "price:1:1:100", "size:1:0:2", "price:1:2:101", "size:1:3:3", "string:1:32:N",
    ]);
    // The bid moves and the rest comes: the bid is not sent again; the end
    // comes with the daily figures, before the book update.
    message(&shared, &crate::types::Quote {
        bid: 99 * s, ask: 101 * s, last: 100 * s, bid_size: 4 * q, ask_size: 3 * q, last_size: q,
        volume: 50 * q, high: 102 * s, low: 97 * s, close: 98 * s, open: 99 * s,
        bid_exch_mask: 1, ..Default::default()
    }, &[MdStep::Last, MdStep::Daily, MdStep::Quote], None);
    let mut w = SnapRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, vec![
        "price:1:4:100", "size:1:5:1", "size:1:8:50", "price:1:6:102", "price:1:7:97", "price:1:9:98",
        "price:1:14:99", "end:1",
    ]);
    // Done: nothing more, and a cancel now is for an unknown request.
    let mut w = SnapRec::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "{:?}", w.events);
    client.cancel_mkt_data(1).unwrap();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:1:300:")), "{:?}", w.events);
    assert_eq!(engine.join().unwrap(), vec!["subscribe:756733:true", "unsubscribe:5"]);
}

/// Records the market data callbacks with the price attribute and the
/// generic ticks (ibx#446).
#[derive(Default)]
struct SeqRec { events: Vec<String> }
impl Wrapper for SeqRec {
    fn tick_price(&mut self, req_id: i64, tt: i32, p: f64, a: &crate::api::types::TickAttrib) {
        self.events.push(format!("price:{req_id}:{tt}:{p}:{}", if a.can_auto_execute { "auto" } else { "-" }));
    }
    fn tick_size(&mut self, req_id: i64, tt: i32, s: f64) { self.events.push(format!("size:{req_id}:{tt}:{s}")); }
    fn tick_string(&mut self, req_id: i64, tt: i32, v: &str) { self.events.push(format!("string:{req_id}:{tt}:{v}")); }
    fn tick_generic(&mut self, req_id: i64, tt: i32, v: f64) { self.events.push(format!("generic:{req_id}:{tt}:{v}")); }
    fn tick_snapshot_end(&mut self, req_id: i64) { self.events.push(format!("end:{req_id}")); }
    fn tick_req_params(&mut self, req_id: i64, min_tick: f64, bbo: &str, perms: i64) {
        self.events.push(format!("params:{req_id}:{min_tick}:{bbo}:{perms}"));
    }
    fn market_data_type(&mut self, req_id: i64, t: i32) { self.events.push(format!("mdt:{req_id}:{t}")); }
}

fn eur_usd() -> Contract {
    Contract { con_id: 12087792, symbol: "EUR".into(), sec_type: "CASH".into(), exchange: "IDEALPRO".into(),
        currency: "USD".into(), ..Default::default() }
}

/// The EUR.USD quote of the captured first message (02/10/2026 08:16
/// Paris).
fn captured_eur_usd_quote() -> crate::types::Quote {
    let p = |raw: i64| raw * crate::types::PRICE_SCALE / 100_000;
    let q = crate::types::QTY_SCALE;
    crate::types::Quote {
        bid: p(112_547), ask: p(112_549), bid_size: 4_000_000 * q, ask_size: 12_000_000 * q,
        last: p(112_550), last_size: 0, volume: 0, close: p(112_430), high: p(112_585), low: p(112_320),
        timestamp_ns: 1_790_921_778 * 1_000_000_000, ..Default::default()
    }
}

/// The captured first EUR.USD message: bid/ask book, last trade with
/// status 0 (its time, then its price), daily figures after the trade;
/// the farm gave no auto-execution flag. `steps`: the message's steps.
fn captured_eur_usd(shared: &SharedState, steps: &[crate::md_events::MdStep], halted: Option<i64>) {
    shared.market.push_tick_req_params(crate::bridge::TickReqParams {
        instrument: 5, min_tick: 0.00001, bbo_exchange: String::new(), snapshot_permissions: 0,
    });
    message(shared, &captured_eur_usd_quote(), steps, halted);
}

// ibx#446, captured 02/10/2026 (EUR.USD snapshots 9460-9462): the market
// data type, then the request parameters; the trade's time and halted
// state, the daily high, low and close, then the bid and the ask with
// their sizes, never executing automatically; no last price for a
// currency pair; the end.
#[test]
fn eur_usd_snapshot_in_the_reference_order() {
    use crate::md_events::MdStep;
    let (client, rx, shared) = test_client();
    let engine = top_engine(rx);
    client.req_mkt_data(1, &eur_usd(), "", true, false).unwrap();
    captured_eur_usd(&shared, &[MdStep::Time, MdStep::Last, MdStep::Daily, MdStep::Quote], Some(0));
    let mut w = SeqRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, vec![
        "mdt:1:1", "params:1:0.00001::0",
        "string:1:45:1790921778", "generic:1:49:0",
        "price:1:6:1.12585:-", "price:1:7:1.1232:-", "price:1:9:1.1243:-",
        "price:1:1:1.12547:-", "size:1:0:4000000", "price:1:2:1.12549:-", "size:1:3:12000000",
        "end:1",
    ]);
    drop(engine);
}

// ibx#446, captured 02/10/2026 (SPY snapshot 9463): the trade's time,
// its price with its size, its halted state, the close, the open (an
// update of its own), then the bid and ask, which execute automatically
// as the farm said. The captured volume is left out: the reference sent
// none for it, for a reason not read yet.
#[test]
fn spy_snapshot_in_the_reference_order() {
    use crate::md_events::{field, MdEvent, MdStep};
    let (client, rx, shared) = test_client();
    let engine = top_engine(rx);
    client.req_mkt_data(1, &spy_stk(), "", true, false).unwrap();
    let p = |raw: i64| raw * crate::types::PRICE_SCALE / 100;
    let q = crate::types::QTY_SCALE;
    shared.market.push_tick_req_params(crate::bridge::TickReqParams {
        instrument: 5, min_tick: 0.01, bbo_exchange: "a60001".into(), snapshot_permissions: 3,
    });
    // The message: book, trade, daily close, daily open.
    let mut last = MdEvent::new(5, MdStep::Last).with(field::LAST, p(76_659)).with(field::LAST_SIZE, 80 * q);
    last.halted = Some(0);
    let mut book = MdEvent::new(5, MdStep::Quote).with(field::BID, p(76_624)).with(field::BID_SIZE, 800 * q)
        .with(field::ASK, p(76_634)).with(field::ASK_SIZE, 1000 * q);
    let mut marks = crate::types::QuoteMarks::default();
    marks.set_auto_bits(12);
    book.auto = marks.auto_word();
    for ev in [
        MdEvent::new(5, MdStep::Time).with(field::TIME, 1_790_921_446 * 1_000_000_000),
        last,
        MdEvent::new(5, MdStep::Daily).with(field::CLOSE, p(76_399)),
        MdEvent::new(5, MdStep::Daily).with(field::OPEN, p(76_442)),
        book,
    ] {
        shared.market.md_events.push_now(&ev);
    }
    let mut w = SeqRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, vec![
        "mdt:1:1", "params:1:0.01:a60001:3",
        "string:1:45:1790921446", "price:1:4:766.59:-", "size:1:5:80", "generic:1:49:0",
        "price:1:9:763.99:-", "price:1:14:764.42:-",
        "price:1:1:766.24:auto", "size:1:0:800", "price:1:2:766.34:auto", "size:1:3:1000",
        "end:1",
    ]);
    drop(engine);
}

/// A SPY quote of 02/10/2026.
fn spy_quote() -> crate::types::Quote {
    let p = |raw: i64| raw * crate::types::PRICE_SCALE / 100;
    let q = crate::types::QTY_SCALE;
    crate::types::Quote {
        bid: p(76_624), ask: p(76_634), bid_size: 800 * q, ask_size: 1000 * q, last: p(76_659), last_size: 80 * q,
        close: p(76_399), open: p(76_442), timestamp_ns: 1_790_921_446 * 1_000_000_000,
        bid_exch_mask: 1, ask_exch_mask: 1, ..Default::default()
    }
}

// ibx#446, from the reference's delayed sender (`jextend.dL.b(List, s,
// int, pa, dy, o, SnapshotPreference, Set)`): a delayed snapshot sends the
// trade's time as 88, never the exchanges nor a halted tick, and ends once
// the delayed bid, ask, last, open, close and time came.
#[test]
fn delayed_snapshot_sends_its_own_time_and_no_exchanges() {
    let (client, rx, shared) = test_client();
    let engine = top_engine(rx);
    client.req_mkt_data(1, &spy_stk(), "", true, false).unwrap();
    client.core.set_delayed(1);
    let message = crate::md_events::TestMessage { halted: Some(0), ..Default::default() };
    shared.market.push_test_message(5, &spy_quote(), &message);
    let mut w = SeqRec::default();
    client.process_msgs(&mut w);
    let ticks: Vec<&str> = w.events.iter().map(String::as_str)
        .filter(|e| !e.starts_with("mdt:") && !e.starts_with("params:")).collect();
    assert_eq!(ticks, vec![
        "string:1:88:1790921446", "price:1:68:766.59:-", "size:1:71:80",
        "price:1:75:763.99:-", "price:1:76:764.42:-",
        "price:1:66:766.24:-", "size:1:69:800", "price:1:67:766.34:-", "size:1:70:1000",
        "end:1",
    ]);
    drop(engine);
}

// ibx#446: a side with no quote is -1 on the wire (captured 28/09/2026);
// the reference sends it as it is: tickPrice -1 with size 0, on a stream
// and in a snapshot.
#[test]
fn empty_quote_side_goes_out_as_minus_one() {
    use crate::md_events::{MdStep, TestMessage};
    for snapshot in [false, true] {
        let (client, rx, shared) = test_client();
        let engine = top_engine(rx);
        client.req_mkt_data(1, &spy_stk(), "", snapshot, false).unwrap();
        let s = crate::types::PRICE_SCALE;
        let message = TestMessage { steps: Some(&[MdStep::Quote]), sizes_seen: true, ..Default::default() };
        shared.market.push_test_message(5, &crate::types::Quote { bid: -s, ask: -s, ..Default::default() }, &message);
        let mut w = SeqRec::default();
        client.process_msgs(&mut w);
        let quote: Vec<&str> = w.events.iter().map(String::as_str)
            .filter(|e| e.starts_with("price:") || e.starts_with("size:")).take(4).collect();
        let auto = if snapshot { "-" } else { "auto" };
        assert_eq!(quote, vec![
            format!("price:1:1:-1:{auto}"), "size:1:0:0".into(), format!("price:1:2:-1:{auto}"), "size:1:3:0".into(),
        ], "snapshot {snapshot}");
        drop(engine);
    }
}

// ibx#446, from the reference's delayed sender: a delayed stream sends the
// trade's time as 88 and a halted state as 90 (when its bits change, then
// in each step until the next book update, as 49), and no exchanges. The
// status comes with the trade's price step, after its time step: the time
// step has no halted state yet. A size goes with its price only, not again
// on its own (captured 05/10/2026, ibx#444: 7203 on delayed data).
#[test]
fn delayed_stream_sends_88_and_90_and_no_exchanges() {
    use crate::md_events::MdStep;
    let (client, rx, shared) = test_client();
    let engine = top_engine(rx);
    client.req_mkt_data(1, &spy_stk(), "", false, false).unwrap();
    client.core.set_delayed(1);
    let quote = crate::types::Quote { close: 0, open: 0, ..spy_quote() };
    message(&shared, &quote, &[MdStep::Time, MdStep::Last, MdStep::Quote], Some(1));
    let mut w = SeqRec::default();
    client.process_msgs(&mut w);
    let ticks: Vec<&str> = w.events.iter().map(String::as_str)
        .filter(|e| !e.starts_with("mdt:") && !e.starts_with("params:")).collect();
    assert_eq!(ticks, vec![
        "string:1:88:1790921446",
        "price:1:68:766.59:-", "size:1:71:80", "generic:1:90:1",
        "price:1:66:766.24:-", "size:1:69:800", "price:1:67:766.34:-", "size:1:70:1000", "generic:1:90:1",
    ]);
    assert!(!w.events.iter().any(|e| e.contains(":32:") || e.contains(":33:") || e.contains(":45:") || e.contains(":49:")),
        "{:?}", w.events);
    drop(engine);
}

// ibx#446: when the daily figures came before the trade in the message,
// they are sent first; a trade with no status gives no halted tick.
#[test]
fn snapshot_daily_figures_first_when_they_came_first() {
    use crate::md_events::MdStep;
    let (client, rx, shared) = test_client();
    let engine = top_engine(rx);
    client.req_mkt_data(1, &eur_usd(), "", true, false).unwrap();
    captured_eur_usd(&shared, &[MdStep::Daily, MdStep::Time, MdStep::Last, MdStep::Quote], None);
    let mut w = SeqRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, vec![
        "mdt:1:1", "params:1:0.00001::0",
        "price:1:6:1.12585:-", "price:1:7:1.1232:-", "price:1:9:1.1243:-", "string:1:45:1790921778",
        "price:1:1:1.12547:-", "size:1:0:4000000", "price:1:2:1.12549:-", "size:1:3:12000000",
        "end:1",
    ]);
    drop(engine);
}

/// The captured EUR.USD stream 9470 (02/10/2026): the first message
/// (trade time, trade price, daily figures, book; status 0; every size
/// given), then five book updates (bid, bid size, ask, ask size); the
/// callbacks each message gives.
fn eur_usd_stream_messages() -> Vec<(crate::types::Quote, Vec<crate::md_events::MdStep>, Vec<&'static str>)> {
    use crate::md_events::MdStep;
    let p = |raw: i64| raw * crate::types::PRICE_SCALE / 100_000;
    let m = |n: i64| n * 1_000_000 * crate::types::QTY_SCALE;
    let mut q = crate::types::Quote {
        bid: p(112_546), ask: p(112_547), bid_size: m(2), ask_size: m(7), last: p(112_550),
        close: p(112_430), high: p(112_585), low: p(112_320), timestamp_ns: 1_790_921_787 * 1_000_000_000,
        ..Default::default()
    };
    let mut out = vec![(q, vec![MdStep::Time, MdStep::Last, MdStep::Daily, MdStep::Quote], vec![
        "string:1:45:1790921787", "price:1:4:1.1255:-", "size:1:5:0", "size:1:5:0",
        "size:1:8:0", "price:1:6:1.12585:-", "price:1:7:1.1232:-", "price:1:9:1.1243:-",
        "price:1:1:1.12546:auto", "size:1:0:2000000", "price:1:2:1.12547:auto", "size:1:3:7000000",
        "size:1:0:2000000", "size:1:3:7000000",
    ])];
    let updates: [(i64, i64, i64, i64, &[&'static str]); 5] = [
        (112_546, 2, 112_547, 6, &["size:1:3:6000000"]),
        (112_546, 3, 112_547, 3, &["size:1:0:3000000", "size:1:3:3000000"]),
        (112_546, 2, 112_547, 4, &["size:1:0:2000000", "size:1:3:4000000"]),
        (112_546, 2, 112_547, 6, &["size:1:3:6000000"]),
        (112_547, 1, 112_549, 19, &[
            "price:1:1:1.12547:auto", "size:1:0:1000000", "price:1:2:1.12549:auto", "size:1:3:19000000",
            "size:1:0:1000000", "size:1:3:19000000",
        ]),
    ];
    for (bid, bid_size, ask, ask_size, want) in updates {
        q.bid = p(bid);
        q.bid_size = m(bid_size);
        q.ask = p(ask);
        q.ask_size = m(ask_size);
        out.push((q, vec![MdStep::Quote], want.to_vec()));
    }
    out
}

// ibx#446, captured 02/10/2026 (EUR.USD stream 9470): the market data
// type before the request parameters; then, for the first message, the
// trade's time, its price with its size, its size again (a first size of
// 0 is sent), the daily volume (0), high, low and close, then the book:
// bid and ask with their sizes, then both sizes again; the bid and the ask
// execute automatically; no halted tick for status 0. Then each book
// update sends only what changed. The same callbacks whether the client
// reads after each message or once after all of them: the reference
// never merges two messages.
#[test]
fn eur_usd_stream_in_the_reference_order() {
    for read_each in [true, false] {
        let (client, rx, shared) = test_client();
        let engine = top_engine(rx);
        client.req_mkt_data(1, &eur_usd(), "", false, false).unwrap();
        shared.market.push_tick_req_params(crate::bridge::TickReqParams {
            instrument: 5, min_tick: 0.00001, bbo_exchange: String::new(), snapshot_permissions: 0,
        });
        let mut want = vec!["mdt:1:1", "params:1:0.00001::0"];
        let mut w = SeqRec::default();
        for (q, steps, callbacks) in eur_usd_stream_messages() {
            let message = crate::md_events::TestMessage {
                steps: Some(&steps), halted: steps.contains(&crate::md_events::MdStep::Last).then_some(0),
                sizes_seen: true, ..Default::default()
            };
            shared.market.push_test_message(5, &q, &message);
            want.extend(callbacks);
            if read_each {
                client.process_msgs(&mut w);
                assert_eq!(w.events, want);
            }
        }
        client.process_msgs(&mut w);
        assert_eq!(w.events, want, "read after each message: {read_each}");
        drop(engine);
    }
}

/// The captured AAPL stream 9440 (28/09/2026, a request on a quote that
/// already had data): the quote it joined, then four messages (steps,
/// quote) and the callbacks each gives.
#[allow(clippy::type_complexity)]
fn aapl_join_messages() -> (crate::types::Quote, Vec<(crate::types::Quote, Vec<crate::md_events::MdStep>, Vec<&'static str>)>) {
    use crate::md_events::MdStep::*;
    let p = |raw: i64| raw * crate::types::PRICE_SCALE / 100;
    let n = |shares: i64| shares * crate::types::QTY_SCALE;
    let (k, pq, q_, v) = (1, 2 | 4, 4, 8);
    let mut q = crate::types::Quote {
        bid: p(34_233), ask: p(34_237), last: p(34_235), bid_size: n(200), ask_size: n(200), last_size: n(40),
        volume: n(140_671), high: p(34_299), low: p(34_017), close: p(34_107), open: p(34_022),
        timestamp_ns: 1_790_604_766 * 1_000_000_000, bid_exch_mask: k | q_, ask_exch_mask: pq, last_exch_mask: 0,
    };
    let joined = q;
    let mut out = Vec::new();
    // 16:12:47.239: volume, then a trade with only its time.
    q.volume = n(140_672);
    q.timestamp_ns = 1_790_604_767 * 1_000_000_000;
    out.push((q, vec![Daily, Time, Last], vec![
        "size:1:8:140672", "generic:1:49:0", "string:1:45:1790604767", "generic:1:49:0", "generic:1:49:0",
    ]));
    // 16:12:47.488: volume, then a trade with only its exchange.
    q.volume = n(140_676);
    out.push((q, vec![Daily, Exchange, Last], vec!["size:1:8:140676", "generic:1:49:0", "generic:1:49:0", "generic:1:49:0"]));
    // 16:12:47.738: volume, a trade with time, exchange and size, the book.
    q.bid_size = n(240);
    q.ask_size = n(240);
    q.bid_exch_mask = k | q_ | v;
    q.volume = n(140_678);
    q.last_size = n(80);
    q.timestamp_ns = 1_790_604_768 * 1_000_000_000;
    out.push((q, vec![Daily, Time, Exchange, Last, Quote], vec![
        "size:1:8:140678", "generic:1:49:0", "string:1:45:1790604768", "generic:1:49:0", "generic:1:49:0",
        "size:1:5:80", "generic:1:49:0", "size:1:0:240", "size:1:3:240", "string:1:32:KQV", "generic:1:49:0",
    ]));
    // 16:12:47.991: after the book update, no halted state any more.
    q.bid = p(34_234);
    q.bid_size = n(160);
    q.bid_exch_mask = k;
    q.volume = n(140_687);
    q.last = p(34_236);
    q.last_size = n(320);
    out.push((q, vec![Daily, Exchange, Last, Quote], vec![
        "size:1:8:140687", "price:1:4:342.36:-", "size:1:5:320", "size:1:5:320",
        "price:1:1:342.34:auto", "size:1:0:160", "size:1:0:160", "string:1:32:K",
    ]));
    (joined, out)
}

// ibx#446, captured 28/09/2026 (AAPL stream 9440, a request on a quote
// that already had data): the first step sends all the quote, with the
// halted state; the halted state then goes out again in each step (daily
// figures, trade time, trade exchange, trade price) until the next book
// update; each message's steps follow its order. The same callbacks
// whether the client reads after each message or once after all of them.
#[test]
fn stock_stream_joining_a_quote_in_the_reference_order() {
    use crate::md_events::{MdStep, TestMessage};
    for read_each in [true, false] {
        let (client, rx, shared) = test_client();
        let engine = top_engine(rx);
        set_exchange_map(&shared, 5, ["K", "P", "Q", "V"].iter().enumerate().map(|(bit, letter)| {
            crate::types::SmartComponent { bit_number: bit as i32, exchange: letter.to_string(), exchange_letter: letter.to_string() }
        }).collect());
        let aapl = Contract { con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(),
            ..Default::default() };
        client.req_mkt_data(1, &aapl, "", false, false).unwrap();
        let (joined, messages) = aapl_join_messages();
        // The quote as the record has it, then the request joins.
        let whole = TestMessage { steps: Some(&[MdStep::CatchUp]), halted: Some(0), sizes_seen: true, ..Default::default() };
        shared.market.push_test_message(5, &joined, &whole);
        client.core.join_stream(1, shared.market.md_events.head());
        let mut want = vec![
            "mdt:1:1",
            "price:1:1:342.33:auto", "size:1:0:200", "price:1:2:342.37:auto", "size:1:3:200",
            "price:1:4:342.35:-", "size:1:5:40", "size:1:0:200", "size:1:3:200", "size:1:5:40", "size:1:8:140671",
            "price:1:6:342.99:-", "price:1:7:340.17:-", "price:1:9:341.07:-", "price:1:14:340.22:-",
            "string:1:32:KQ", "string:1:33:PQ", "string:1:45:1790604766", "generic:1:49:0",
        ];
        let mut w = SeqRec::default();
        if read_each {
            client.process_msgs(&mut w);
            assert_eq!(w.events, want);
        }
        for (q, steps, callbacks) in messages {
            let message = TestMessage { steps: Some(&steps), sizes_seen: true, ..Default::default() };
            shared.market.push_test_message(5, &q, &message);
            want.extend(callbacks);
            if read_each {
                client.process_msgs(&mut w);
                assert_eq!(w.events, want);
            }
        }
        client.process_msgs(&mut w);
        assert_eq!(w.events, want, "read after each message: {read_each}");
        drop(engine);
    }
}

// ibx#446: two messages read at once are not merged: each one's values go
// out, in order (the old dispatch sent the last values only, and lost a
// halted state repeated in a step with nothing else).
#[test]
fn messages_read_at_once_are_not_merged() {
    use crate::md_events::MdStep;
    let (client, rx, shared) = test_client();
    let engine = top_engine(rx);
    client.req_mkt_data(1, &spy_stk(), "", false, false).unwrap();
    let s = crate::types::PRICE_SCALE;
    let q = crate::types::QTY_SCALE;
    let mut quote = crate::types::Quote { bid: 100 * s, ask: 101 * s, bid_size: q, ask_size: q, ..Default::default() };
    message(&shared, &quote, &[MdStep::Quote], None);
    quote.bid = 99 * s;
    message(&shared, &quote, &[MdStep::Quote], None);
    quote.bid = 100 * s;
    message(&shared, &quote, &[MdStep::Quote], None);
    // A halted trade with only its time, then one with its exchange.
    quote.timestamp_ns = 1_790_000_000 * 1_000_000_000;
    message(&shared, &quote, &[MdStep::Time, MdStep::Last], Some(1));
    message(&shared, &quote, &[MdStep::Exchange, MdStep::Last], None);
    let mut w = SeqRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, vec![
        "mdt:1:1",
        "price:1:1:100:auto", "size:1:0:1", "price:1:2:101:auto", "size:1:3:1", "size:1:0:1", "size:1:3:1",
        "price:1:1:99:auto", "size:1:0:1",
        "price:1:1:100:auto", "size:1:0:1",
        "string:1:45:1790000000", "generic:1:49:1",
        "generic:1:49:1", "generic:1:49:1",
    ]);
    drop(engine);
}

// ibx#446: an option on an exchange that says it per quote executes
// automatically only as the farm said.
#[test]
fn option_stream_auto_execution_comes_from_the_farm() {
    let (client, rx, shared) = test_client();
    let engine = top_engine(rx);
    let option = Contract { con_id: 900_000_001, symbol: "SPY".into(), sec_type: "OPT".into(),
        exchange: "SMART".into(), ..Default::default() };
    client.req_mkt_data(1, &option, "", false, false).unwrap();
    let s = crate::types::PRICE_SCALE;
    shared.market.push_test_message(5, &crate::types::Quote { bid: s, ask: 2 * s, ..Default::default() }, &Default::default());
    let mut w = SeqRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, vec!["mdt:1:1", "price:1:1:1:-", "size:1:0:0", "price:1:2:2:-", "size:1:3:0"]);
    let message = crate::md_events::TestMessage { auto_bits: Some(4), ..Default::default() };
    shared.market.push_test_message(5, &crate::types::Quote { bid: 3 * s, ask: 4 * s, ..Default::default() }, &message);
    let mut w = SeqRec::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, vec!["price:1:1:3:auto", "size:1:0:0", "price:1:2:4:-", "size:1:3:0"]);
    drop(engine);
}

// A snapshot that never completes ends 11 s after its start; a stream
// never ends.
#[test]
fn plain_snapshot_ends_at_the_time_limit() {
    let start = std::time::Instant::now();
    let (client, rx, shared) = test_client();
    let engine = top_engine(rx);
    client.req_mkt_data(1, &spy_stk(), "", true, false).unwrap();
    let s = crate::types::PRICE_SCALE;
    shared.market.push_test_message(5, &crate::types::Quote { bid: 100 * s, ..Default::default() }, &Default::default());
    let mut w = SnapRec::default();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("end:")), "{:?}", w.events);
    let mut out = Vec::new();
    client.core.poll_market_data(&shared, Some(start + std::time::Duration::from_millis(5_000)), &mut out);
    assert!(out.is_empty(), "{out:?}");
    client.core.poll_market_data(&shared, Some(start + crate::control::snapshot::TIMEOUT + std::time::Duration::from_millis(500)), &mut out);
    assert_eq!(out, [crate::client_core::MdOut::SnapshotEnd(1)]);
    out.clear();
    client.core.poll_market_data(&shared, Some(start + 2 * crate::control::snapshot::TIMEOUT), &mut out);
    assert!(out.is_empty(), "gone after its end: {out:?}");
    drop(engine);
}

// A snapshot with legal generic ticks is refused with 321, before the
// duplicate check; a list with an unknown tick, or a stream, goes on.
#[test]
fn plain_snapshot_with_generic_ticks_is_refused() {
    let (client, rx, _shared) = test_client();
    let engine = top_engine(rx);
    client.req_mkt_data(1, &spy_stk(), "233", true, false).unwrap();
    client.req_mkt_data(2, &spy_stk(), "233,13", true, false).unwrap();
    // The same id again, with generic ticks: 321, not the duplicate 322.
    client.req_mkt_data(2, &spy_stk(), "236", true, false).unwrap();
    let mut w = SnapRec::default();
    client.process_msgs(&mut w);
    let refusal = "Error validating request.-'bQ' : cause - Snapshot market data subscription is not applicable to generic ticks";
    assert_eq!(w.events.iter().filter(|e| e.starts_with("error:")).cloned().collect::<Vec<_>>(),
        vec![format!("error:1:321:{refusal}"), format!("error:2:321:{refusal}")]);
    assert_eq!(engine.join().unwrap(), vec!["subscribe:756733:true"]);
}

// The per-second limit is the API ticker limit of the logon; beyond it a
// snapshot is refused with 321 and the reference's text.
#[test]
fn plain_snapshot_beyond_the_limit_is_refused() {
    let (client, rx, shared) = test_client();
    let engine = top_engine(rx);
    shared.reference.set_snapshot_rate_limit(1);
    for req_id in 10..15 {
        let mut c = spy_stk();
        c.con_id = 756733 + req_id;
        client.req_mkt_data(req_id, &c, "", true, false).unwrap();
    }
    let mut w = SnapRec::default();
    client.process_msgs(&mut w);
    let text = "Error validating request.-'bQ' : cause - Snapshot requests limitation exceeded:1 per 1 second(s)";
    let refused = w.events.iter().filter(|e| e.ends_with(&format!(":321:{text}"))).count();
    // Five requests span at most two seconds: at most two go.
    assert!(refused >= 3, "{:?}", w.events);
    let asked = engine.join().unwrap().iter().filter(|e| e.starts_with("subscribe:")).count();
    assert_eq!(asked + refused, 5);
}

// ibx#458: the news tick of a generic tick list, as the reference's parser
// reads it.
#[test]
fn news_tick_of_a_generic_tick_list() {
    use crate::client_core::{news_tick, NewsTick};
    assert_eq!(news_tick(""), NewsTick::None);
    assert_eq!(news_tick("mdoff,233"), NewsTick::None);
    assert_eq!(news_tick("mdoff,292"), NewsTick::Default);
    assert_eq!(news_tick(" 233 , 292 "), NewsTick::Default);
    assert_eq!(news_tick("mdoff,292:BRFG+DJNL"), NewsTick::Codes("BRFG+DJNL".into()));
    assert_eq!(news_tick("mdoff,292:"), NewsTick::Invalid);
    assert_eq!(news_tick("1292,2920"), NewsTick::None);
}

// ibx#458: the provider key and the 10094 texts (captured 02/10/2026).
#[test]
fn news_provider_key_and_refusals() {
    use crate::client_core::news_providers;
    let sources: Vec<String> = ["DJNL", "BRFG", "DJ-RTPRO", "DJ-N", "BRFUPDN", "DJ-RTA", "DJ-RTE", "DJ-RTG"]
        .iter().map(|s| s.to_string()).collect();
    assert_eq!(news_providers("STK", None, &sources).unwrap().join(","),
        "BRFG,BRFUPDN,DJ-N,DJ-RTA,DJ-RTE,DJ-RTG,DJ-RTPRO,DJNL");
    assert_eq!(news_providers("STK", Some("DJNL+BRFG"), &sources).unwrap().join(","), "BRFG,DJNL");
    assert_eq!(news_providers("STK", Some("dj-n+DJ-N+"), &sources).unwrap().join(","), "DJ-N");
    assert_eq!(news_providers("STK", Some("XYZ"), &sources).unwrap_err(),
        "API News error:Source code unchecked in API news Settings: XYZ");
    assert_eq!(news_providers("STK", Some("XYZ+BRFG+ABC"), &sources).unwrap_err(),
        "API News error:Source code unchecked in API news Settings: XYZ,Source code unchecked in API news Settings: ABC");
    for sec_type in ["FUT", "OPT", "FOP", "WAR", "IOPT", "CFD", "FWD", "SLB", "ICS"] {
        assert_eq!(news_providers(sec_type, None, &sources).unwrap_err(),
            "API News error:Derivative contracts cannot be used to subscribe to news, please use the underlying \
             (Stocks, Cash, News Topics, and certain Indexes are supported).");
    }
    assert!(news_providers("IND", None, &sources).is_ok());
    assert!(news_providers("CASH", Some("BRFG"), &sources).is_ok());
}

// ibx#458: a refused news tick of a contract with a conId ends the request
// with 10094 at once, nothing sent (captured: MNQ future, "292:XYZ").
#[test]
fn news_tick_refusals_of_a_known_contract() {
    let (client, rx, shared) = test_client();
    shared.reference.set_news_sources(vec!["BRFG".into(), "DJ-N".into()]);
    let mnq = Contract { con_id: 815824267, symbol: "MNQ".into(), sec_type: "FUT".into(), exchange: "CME".into(), ..Default::default() };
    client.req_mkt_data(9572, &mnq, "mdoff,292", false, false).unwrap();
    let nvda = Contract { con_id: 4815747, symbol: "NVDA".into(), sec_type: "STK".into(), exchange: "SMART".into(), ..Default::default() };
    client.req_mkt_data(9583, &nvda, "mdoff,292:XYZ", false, false).unwrap();
    assert!(rx.try_recv().is_err(), "nothing sent");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, [
        "error:9572:10094:API News error:Derivative contracts cannot be used to subscribe to news, please use the underlying (Stocks, Cash, News Topics, and certain Indexes are supported).",
        "error:9583:10094:API News error:Source code unchecked in API news Settings: XYZ",
    ], "{:?}", w.events);
    assert!(client.core.req_to_instrument.lock().unwrap().is_empty(), "the ids are free");
}

// ibx#458: an accepted news tick follows the request's subscription with
// the provider key; a refusal of a contract without a conId waits for the
// lookup in the engine; cancelMktData ends both.
#[test]
fn news_tick_follows_the_subscription() {
    let (client, rx, shared) = test_client();
    shared.reference.set_news_sources(vec!["DJNL".into(), "BRFG".into(), "DJ-N".into()]);
    let engine = std::thread::spawn(move || {
        let mut got = Vec::new();
        while let Ok(cmd) = rx.recv_timeout(std::time::Duration::from_millis(500)) {
            match &cmd {
                ControlCommand::Subscribe { reply_tx: Some(r), .. } => { let _ = r.send(Ok(5)); }
                ControlCommand::SubscribeBySymbol { reply_tx: Some(r), .. } => { let _ = r.send(Ok(6)); }
                _ => {}
            }
            got.push(cmd);
        }
        got
    });
    let aapl = Contract { con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), ..Default::default() };
    client.req_mkt_data(9560, &aapl, "mdoff,292:DJNL+BRFG", false, false).unwrap();
    let nvda = Contract { symbol: "NVDA".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    client.req_mkt_data(9583, &nvda, "mdoff,292:XYZ", false, false).unwrap();
    client.cancel_mkt_data(9560).unwrap();
    let got = engine.join().unwrap();
    let news: Vec<(InstrumentId, i64, String, Option<String>)> = got.iter().filter_map(|c| match c {
        ControlCommand::SubscribeNews { instrument, con_id, providers, refusal, .. } =>
            Some((*instrument, *con_id, providers.clone(), refusal.clone())),
        _ => None,
    }).collect();
    assert_eq!(news, [
        (5, 265598, "BRFG,DJNL".to_string(), None),
        (6, 0, String::new(), Some("API News error:Source code unchecked in API news Settings: XYZ".to_string())),
    ]);
    let subscribe = got.iter().position(|c| matches!(c, ControlCommand::Subscribe { .. })).unwrap();
    let first_news = got.iter().position(|c| matches!(c, ControlCommand::SubscribeNews { .. })).unwrap();
    assert!(subscribe < first_news, "{got:?}");
    assert!(matches!(got.last(), Some(ControlCommand::Unsubscribe { instrument: 5 })), "{got:?}");
}

// ibx#458: a request refused once its contract was found reports 10094
// and is gone.
#[test]
fn news_refusal_after_the_lookup_is_reported() {
    let (client, rx, shared) = test_client();
    client.core.req_to_instrument.lock().unwrap().insert(9583, 6);
    client.core.instrument_to_req.lock().unwrap().insert(6, vec![9583]);
    shared.market.push_md_reject(crate::bridge::MdReject::NewsRefused {
        instrument: 6, con_id: 265598, text: "API News error:Source code unchecked in API news Settings: XYZ".into() });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.contains(&"error:9583:10094:API News error:Source code unchecked in API news Settings: XYZ".to_string()), "{:?}", w.events);
    assert!(!client.core.req_to_instrument.lock().unwrap().contains_key(&9583));
    assert!(rx.try_iter().any(|c| matches!(c, ControlCommand::Unsubscribe { instrument: 6 })));
}

// ── ibx#421: values of the logon reply ──

// The current time is the local clock plus the offset of the logon.
#[test]
fn req_current_time_adds_the_logon_clock_offset() {
    #[derive(Default)]
    struct Time(Vec<i64>);
    impl Wrapper for Time {
        fn current_time(&mut self, time: i64) { self.0.push(time); }
    }
    let (client, _rx, shared) = test_client();
    shared.reference.clock().set(60_000);
    let mut w = Time::default();
    client.req_current_time(&mut w);
    let local = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    assert_eq!(w.0.len(), 1);
    assert!((w.0[0] - local - 60).abs() <= 1, "{} vs local {}", w.0[0], local);
}

// Without SECDEFTA in the logon feature list, a matching symbols request
// is refused with 321 before the pattern checks, and nothing is sent.
#[test]
fn req_matching_symbols_refused_without_the_feature() {
    let (client, rx, shared) = test_client();
    shared.reference.set_api_features(crate::control::logon::ApiFeatures::parse("APIELOG"));
    client.req_matching_symbols(8, "AAPL").unwrap();
    client.req_matching_symbols(9, "").unwrap();
    assert!(rx.try_recv().is_err(), "nothing is sent");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let errors: Vec<String> = w.events.iter().filter(|e| e.starts_with("error:")).cloned().collect();
    let refused = "Error validating request.-'ce' : cause - Failed to request matching symbols";
    assert_eq!(errors, [format!("error:8:321:{refused}"), format!("error:9:321:{refused}")]);

    shared.reference.set_api_features(crate::control::logon::ApiFeatures::parse("APIELOG,SECDEFTA"));
    client.req_matching_symbols(10, "AAPL").unwrap();
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::FetchMatchingSymbols { req_id: 10, .. }));
}

// A duration in years above the logon limit is refused; NIGHTLY lifts it.
#[test]
fn req_historical_data_years_above_the_logon_limit() {
    let (client, rx, shared) = test_client();
    shared.reference.set_max_backfill_years(1);
    client.req_historical_data(5, &spy(), "", "2 Y", "1 day", "TRADES", true, 1, false).unwrap();
    assert!(rx.try_recv().is_err());
    assert_eq!(shared.reference.drain_historical_errors(), vec![(5, 321,
        "Error validating request.-'bM' : cause - Historical data request for 2 year(s) rejected. Max API Backfill Years=1".to_string())]);
    client.req_historical_data(6, &spy(), "", "1 Y", "1 day", "TRADES", true, 1, false).unwrap();
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::FetchHistorical { req_id: 6, .. }));
    shared.reference.set_api_features(crate::control::logon::ApiFeatures::parse("NIGHTLY"));
    client.req_historical_data(7, &spy(), "", "2 Y", "1 day", "TRADES", true, 1, false).unwrap();
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::FetchHistorical { req_id: 7, .. }));
}

// The version cutoff warning reaches the client as error 2172, id -1.
#[test]
fn version_cutoff_warning_reaches_the_client() {
    let (client, _rx, shared) = test_client();
    crate::gateway::apply_first_logon(&Default::default(), Some("10411"), Some("20261201"), 199, &shared);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:-1:2172:The version of the application you are running, 1040.1,")), "{:?}", w.events);
}

// ═══════════════════════════════════════════════════════════════════
//  Several requests on one contract (ibx#444)
// ═══════════════════════════════════════════════════════════════════

/// An engine that answers each subscription with instrument 5 and records
/// the market data commands.
fn sharing_engine(rx: crossbeam_channel::Receiver<ControlCommand>) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        while let Ok(cmd) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            match cmd {
                ControlCommand::Subscribe { reply_tx: Some(tx), con_id, .. } => {
                    let _ = tx.send(Ok(5));
                    seen.push(format!("subscribe:{con_id}"));
                }
                ControlCommand::SubscribeBySymbol { reply_tx: Some(tx), symbol, .. } => {
                    let _ = tx.send(Ok(if seen.iter().any(|s| s.starts_with("lookup")) { 6 } else { 5 }));
                    seen.push(format!("lookup:{symbol}"));
                }
                ControlCommand::SubscribeNews { instrument, providers, .. } => seen.push(format!("news:{instrument}:{providers}")),
                ControlCommand::UnsubscribeNews { instrument, providers } => seen.push(format!("release:{instrument}:{providers}")),
                ControlCommand::Unsubscribe { instrument } => seen.push(format!("unsubscribe:{instrument}")),
                _ => {}
            }
        }
        seen
    })
}

const ALL_NEWS: &str = "BRFG,BRFUPDN,DJ-N,DJ-RTA,DJ-RTE,DJ-RTG,DJ-RTPRO,DJNL";

fn news_sources(shared: &SharedState) {
    shared.reference.set_news_sources(ALL_NEWS.split(',').map(String::from).collect());
}

fn aapl_stk() -> Contract {
    Contract { con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), ..Default::default() }
}

/// The four headlines the first AAPL request got (b1_458_news_dup,
/// 02/10/2026), in the order they came.
fn captured_aapl_news(shared: &SharedState, instrument: InstrumentId) {
    let math = "The New Math of AI: Are Those Trillion-Dollar Numbers for Real? -- Barrons.com";
    for (t, p, a, h, x) in [
        (1790893800000, "DJ-RTPRO", "DJ-RTPRO$1f790db1", "VP Newstead Sells 2,399 Of Apple Inc >AAPL", "A:800015:L:en"),
        (1790920800000, "DJ-N", "DJ-N$1f798bd2", math, "L:en:A:800015"),
        (1790920800000, "DJ-RTG", "DJ-RTG$1f798bd2", math, "L:en:A:800015"),
        (1790920800000, "DJ-RTPRO", "DJ-RTPRO$1f798bd2", math, "L:en:A:800015"),
    ] {
        shared.market.push_tick_news(TickNews {
            instrument, timestamp: t, provider_code: p.into(), article_id: a.into(), headline: h.into(), extra_data: x.into(),
        });
    }
}

// ibx#444, captured 02/10/2026 (b1_458_news_dup): two reqMktData
// "mdoff,292" on AAPL from one client, no error; the second sent nothing to
// the farm and got at once marketDataType 1, tickReqParams and the four
// headlines the first had. A cancel of one of them sends nothing to the
// farm for the top of book; the last cancel ends the subscription.
#[test]
fn two_requests_on_one_contract_share_the_subscription() {
    let (client, rx, shared) = test_client();
    news_sources(&shared);
    let engine = sharing_engine(rx);
    client.req_mkt_data(9580, &aapl_stk(), "mdoff,292", false, false).unwrap();
    shared.market.push_tick_req_params(crate::bridge::TickReqParams {
        instrument: 5, min_tick: 0.01, bbo_exchange: "9c0001".into(), snapshot_permissions: 3,
    });
    captured_aapl_news(&shared, 5);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events.iter().filter(|e| e.contains(":9580:")).count(), 6, "{:?}", w.events);

    client.req_mkt_data(9581, &aapl_stk(), "mdoff,292", false, false).unwrap();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let math = "The New Math of AI: Are Those Trillion-Dollar Numbers for Real? -- Barrons.com";
    assert_eq!(w.events, [
        "market_data_type:9581:1".to_string(),
        "tick_req_params:9581:0.01:9c0001:3".to_string(),
        "tick_news:9581:1790893800000:DJ-RTPRO:DJ-RTPRO$1f790db1:VP Newstead Sells 2,399 Of Apple Inc >AAPL:A:800015:L:en".to_string(),
        format!("tick_news:9581:1790920800000:DJ-N:DJ-N$1f798bd2:{math}:L:en:A:800015"),
        format!("tick_news:9581:1790920800000:DJ-RTG:DJ-RTG$1f798bd2:{math}:L:en:A:800015"),
        format!("tick_news:9581:1790920800000:DJ-RTPRO:DJ-RTPRO$1f798bd2:{math}:L:en:A:800015"),
    ]);

    // A new headline reaches both.
    shared.market.push_tick_news(TickNews {
        instrument: 5, timestamp: 1790931600000, provider_code: "DJ-N".into(), article_id: "DJ-N$1f79d3bf".into(),
        headline: "x".into(), extra_data: String::new(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events.iter().filter(|e| e.starts_with("tick_news:")).count(), 2, "{:?}", w.events);

    client.cancel_mkt_data(9580).unwrap();
    client.cancel_mkt_data(9581).unwrap();
    let seen = engine.join().unwrap();
    assert_eq!(seen, [
        "subscribe:265598".to_string(), format!("news:5:{ALL_NEWS}"),
        format!("news:5:{ALL_NEWS}"), format!("release:5:{ALL_NEWS}"),
        "unsubscribe:5".to_string(),
    ]);
}

// ibx#444: a request that joins a quote with data gets all of it at its
// first poll; both requests then get each change.
#[test]
fn a_joining_stream_gets_the_quote_at_once() {
    let (client, rx, shared) = test_client();
    let engine = sharing_engine(rx);
    client.req_mkt_data(1, &aapl_stk(), "", false, false).unwrap();
    let s = crate::types::PRICE_SCALE;
    let q = crate::types::QTY_SCALE;
    let mut quote = crate::types::Quote { bid: 100 * s, ask: 101 * s, bid_size: 2 * q, ask_size: 3 * q, ..Default::default() };
    shared.market.push_test_message(5, &quote, &Default::default());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "tick_price:1:1:100"), "{:?}", w.events);

    client.req_mkt_data(2, &aapl_stk(), "", false, false).unwrap();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "tick_price:2:1:100") && w.events.iter().any(|e| e == "tick_price:2:2:101"), "{:?}", w.events);
    assert!(!w.events.iter().any(|e| e.contains(":1:1:100")), "nothing new for the first: {:?}", w.events);

    quote.bid = 99 * s;
    shared.market.push_test_message(5, &quote, &Default::default());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "tick_price:1:1:99") && w.events.iter().any(|e| e == "tick_price:2:1:99"), "{:?}", w.events);
    client.cancel_mkt_data(2).unwrap();
    let seen = engine.join().unwrap();
    assert_eq!(seen, ["subscribe:265598"], "the first request keeps the subscription");
}

// ibx#444, captured 02/10/2026: the second symbol-only AAPL request was
// looked up (35=c) and then joined the first one's subscription. The engine
// says so once the lookup found the contract; the request moves to the
// running subscription, gets what it has, and its own slot is freed.
#[test]
fn a_symbol_only_request_joins_the_contract_it_finds() {
    let (client, rx, shared) = test_client();
    news_sources(&shared);
    let engine = sharing_engine(rx);
    let aapl = Contract { symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    client.req_mkt_data(9580, &aapl, "mdoff,292", false, false).unwrap();
    shared.market.push_tick_req_params(crate::bridge::TickReqParams {
        instrument: 5, min_tick: 0.01, bbo_exchange: "9c0001".into(), snapshot_permissions: 3,
    });
    client.process_msgs(&mut RecordingWrapper::default());
    client.req_mkt_data(9581, &aapl, "mdoff,292", false, false).unwrap();
    shared.market.push_md_merge(6, 5);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["market_data_type:9581:1", "tick_req_params:9581:0.01:9c0001:3"]);
    assert_eq!(client.core.instrument_to_req.lock().unwrap().get(&5), Some(&vec![9580, 9581]));
    assert!(!client.core.instrument_to_req.lock().unwrap().contains_key(&6));
    client.cancel_mkt_data(9581).unwrap();
    let seen = engine.join().unwrap();
    assert_eq!(seen, [
        "lookup:AAPL".to_string(), format!("news:5:{ALL_NEWS}"),
        "lookup:AAPL".to_string(), format!("news:6:{ALL_NEWS}"),
        "unsubscribe:6".to_string(), format!("release:5:{ALL_NEWS}"),
    ]);
}

// ibx#444 (`jextend.s.a(dy,ec,Set)@116-188`): a request joining a contract
// that runs on delayed data gets 10168 when the client has not enabled
// delayed data, and is not kept; with delayed data enabled it joins on
// delayed data (marketDataType 3).
#[test]
fn joining_a_delayed_subscription() {
    let (client, rx, shared) = test_client();
    let engine = sharing_engine(rx);
    client.req_market_data_type(3);
    client.req_mkt_data(1, &aapl_stk(), "", false, false).unwrap();
    shared.market.push_md_reject(crate::bridge::MdReject::Delayed { instrument: 5 });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:1:10167:")), "{:?}", w.events);

    client.req_market_data_type(1);
    client.req_mkt_data(2, &aapl_stk(), "", false, false).unwrap();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["error:2:10168:Requested market data is not subscribed. Delayed market data is not enabled"]);
    assert!(!client.core.req_to_instrument.lock().unwrap().contains_key(&2));

    client.req_market_data_type(4);
    client.req_mkt_data(3, &aapl_stk(), "", false, false).unwrap();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["market_data_type:3:3"]);
    assert!(client.core.delayed_reqs.lock().unwrap().contains(&3));
    drop(client);
    let seen = engine.join().unwrap();
    assert_eq!(seen, ["subscribe:265598"]);
}

// ibx#447 (`jextend.v.a(boolean)`, captured 07/10/2026): when the
// contract's data is frozen its requests go on with the frozen slot,
// marketDataType 2; a request that joins then is on the frozen slot too;
// back on real-time data they return to the real-time slot, marketDataType
// 1, and the frozen slot is let go.
#[test]
fn a_frozen_contract_moves_its_requests_to_the_frozen_slot_and_back() {
    let (client, rx, shared) = test_client();
    let engine = sharing_engine(rx);
    client.req_market_data_type(2);
    client.req_mkt_data(1, &aapl_stk(), "", false, false).unwrap();
    shared.market.push_md_reject(crate::bridge::MdReject::Frozen { instrument: 5, frozen: 9 });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["market_data_type:1:2"]);
    assert_eq!(client.core.req_to_instrument.lock().unwrap().get(&1), Some(&9));
    assert!(!client.core.instrument_to_req.lock().unwrap().contains_key(&5));

    client.req_mkt_data(2, &aapl_stk(), "", false, false).unwrap();
    assert_eq!(client.core.req_to_instrument.lock().unwrap().get(&2), Some(&9), "joins the frozen slot");
    assert_eq!(client.core.check_mdt_needed(2, true), Some(2));

    shared.market.push_md_reject(crate::bridge::MdReject::Live { instrument: 9, live: 5 });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["market_data_type:1:1", "market_data_type:2:1"]);
    assert_eq!(client.core.instrument_to_req.lock().unwrap().get(&5), Some(&vec![1, 2]));
    assert!(client.core.md_frozen.lock().unwrap().is_empty());

    client.cancel_mkt_data(1).unwrap();
    client.cancel_mkt_data(2).unwrap();
    drop(client);
    let seen = engine.join().unwrap();
    assert_eq!(seen, ["subscribe:265598", "unsubscribe:9", "unsubscribe:5"]);
}

// ibx#444 (`jextend.s.b(boolean,boolean)`, `jextend.ba.a(...)@280`): when
// the top of book is rejected, a request with the news tick keeps it, with
// 2117; one without ends with 354, and the others' subscription stays.
#[test]
fn a_top_reject_keeps_the_news_of_a_request() {
    let (client, rx, shared) = test_client();
    news_sources(&shared);
    let engine = sharing_engine(rx);
    client.req_mkt_data(1, &aapl_stk(), "mdoff,292", false, false).unwrap();
    client.req_mkt_data(2, &aapl_stk(), "", false, false).unwrap();
    shared.market.push_md_reject(crate::bridge::MdReject::NotSubscribed {
        instrument: 5, delayed_available: true, needs_api_subscription: false, description: String::new(), kept_params: None,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, [
        "error:1:2117:Requested top market data is not subscribed. Subscription-independent ticks are still active.292; ".to_string(),
        format!("error:2:354:{}Delayed market data is available.", crate::client_core::MD_NOT_SUBSCRIBED),
    ]);
    assert_eq!(client.core.instrument_to_req.lock().unwrap().get(&5), Some(&vec![1]));
    client.cancel_mkt_data(1).unwrap();
    let seen = engine.join().unwrap();
    assert_eq!(seen, ["subscribe:265598".to_string(), format!("news:5:{ALL_NEWS}"), "unsubscribe:5".to_string()]);
}

// ibx#450, captured 02/10/2026 (GOOGL, "mdoff,292:"): an invalid generic
// tick list of a stream is refused with 321 before anything else, with
// the legal ticks of the first refused list's security type, for every
// later refusal too, as the reference caches the text.
#[test]
fn an_invalid_generic_tick_list_is_refused() {
    let (client, rx, _shared) = test_client();
    let googl = Contract { symbol: "GOOGL".into(), sec_type: "STK".into(), exchange: "SMART".into(), ..Default::default() };
    client.req_mkt_data(9570, &googl, "mdoff,292:", false, false).unwrap();
    // A duplicate id is checked after the list.
    client.core.req_to_instrument.lock().unwrap().insert(7, 0);
    client.req_mkt_data(7, &eur_usd(), "999", false, false).unwrap();
    client.req_mkt_data(8, &spy(), "mdoff", false, false).unwrap();
    assert!(rx.try_recv().is_err(), "nothing sent");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let legal = crate::control::generic_tick::legal_ones("STK");
    assert_eq!(w.events, [
        format!("error:9570:321:Error validating request.-'bQ' : cause - Incorrect generic tick list of mdoff,292:.  Legal ones for (STK) are: {legal}"),
        format!("error:7:321:Error validating request.-'bQ' : cause - Incorrect generic tick list of 999.  Legal ones for (CASH) are: {legal}"),
        format!("error:8:321:Error validating request.-'bQ' : cause - Incorrect generic tick list of mdoff.  Legal ones for (STK) are: {legal}"),
    ]);
}

// ibx#450: a valid list goes on (its ticks other than the news are not
// sent); a snapshot with an invalid list is not refused by this check.
// ibx#444: `mdoff` (any case, anywhere in the list) turns the top of book
// off for its request: the reference's sender skips the top pass of that
// subscriber (`jextend.dL.a(s,int,pa,Map,Set)@136-220`). Another request
// of the same contract still gets it.
#[test]
fn mdoff_request_gets_no_top_of_book() {
    let (client, rx, shared) = test_client();
    let engine = top_engine(rx);
    client.req_mkt_data(1, &spy_stk(), "233,MdOff", false, false).unwrap();
    client.req_mkt_data(2, &spy_stk(), "", false, false).unwrap();
    let mut q = Quote::default();
    q.bid = 150 * PRICE_SCALE;
    q.ask = 151 * PRICE_SCALE;
    q.last = 150 * PRICE_SCALE;
    shared.market.push_test_message(5, &q, &Default::default());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("tick_price:1:") || e.starts_with("tick_size:1:")), "{:?}", w.events);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:2:1:150")), "{:?}", w.events);
    client.cancel_mkt_data(1).unwrap();
    assert!(!client.core.md_top_off.lock().unwrap().contains(&1));
    let _ = engine.join();
}

#[test]
fn a_valid_generic_tick_list_goes_on() {
    let (client, rx, _shared) = test_client();
    let engine = top_engine(rx);
    client.req_mkt_data(1, &spy_stk(), "233,236,mdoff", false, false).unwrap();
    client.req_mkt_data(2, &eur_usd(), "999", true, false).unwrap();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().all(|e| !e.contains(":321:")), "{:?}", w.events);
    let seen = engine.join().unwrap();
    assert_eq!(seen, ["subscribe:756733:false", "subscribe:12087792:true"]);
}

// An engine that gives each new subscription its own slot (10, 11, ...)
// and notes each subscribe with its mode.
fn line_engine(rx: crossbeam_channel::Receiver<ControlCommand>) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut next = 10;
        while let Ok(cmd) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            match cmd {
                ControlCommand::Subscribe { reply_tx: Some(tx), con_id, mode_9887, .. } => {
                    let _ = tx.send(Ok(next));
                    next += 1;
                    seen.push(format!("subscribe:{con_id}:{mode_9887}"));
                }
                ControlCommand::Unsubscribe { instrument } => seen.push(format!("unsubscribe:{instrument}")),
                _ => {}
            }
        }
        seen
    })
}

// ibx#444 (`jextend.s.a(dy,ec,Set)@104-191`): after a 354 with delayed data
// available, the contract's record keeps that state. A new request of a
// client without delayed data gets 10168 at once, nothing sent; with
// delayed data enabled it goes delayed at once: marketDataType 3, no
// 10167, the delayed entries. When that delayed subscription ends, the
// mark goes and the next request asks for live data again.
#[test]
fn a_contract_known_delayed_available_gives_10168_or_goes_delayed() {
    let (client, rx, shared) = test_client();
    let engine = line_engine(rx);
    client.req_mkt_data(1, &aapl_stk(), "", false, false).unwrap();
    shared.market.push_md_reject(crate::bridge::MdReject::NotSubscribed {
        instrument: 10, delayed_available: true, needs_api_subscription: false, description: String::new(), kept_params: None,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, [format!("error:1:354:{}Delayed market data is available.", crate::client_core::MD_NOT_SUBSCRIBED)]);

    let mut w = RecordingWrapper::default();
    client.req_mkt_data(2, &aapl_stk(), "", false, false).unwrap();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["error:2:10168:Requested market data is not subscribed. Delayed market data is not enabled"]);
    assert!(client.core.req_to_instrument.lock().unwrap().get(&2).is_none(), "the request is gone");

    client.req_market_data_type(3);
    let mut w = RecordingWrapper::default();
    client.req_mkt_data(3, &aapl_stk(), "", false, false).unwrap();
    client.process_msgs(&mut w);
    assert_eq!(w.events, ["market_data_type:3:3"]);
    assert!(client.core.delayed_reqs.lock().unwrap().contains(&3));

    client.cancel_mkt_data(3).unwrap();
    client.req_market_data_type(1);
    client.req_mkt_data(4, &aapl_stk(), "", false, false).unwrap();
    let seen = engine.join().unwrap();
    assert_eq!(seen, ["subscribe:265598:0", "unsubscribe:10", "subscribe:265598:1", "unsubscribe:11", "subscribe:265598:0"]);
}

// ibx#444 (`jclient.k_.a(boolean)`, `jextend.a4.a(jclient.record.dU)@136`):
// with every API line taken, a stream on a new contract gets 101 and waits;
// a snapshot and a stream joining a running contract take no line. When a
// line is set free the waiting request is subscribed, with no second 101.
// A waiting request is cancelled with nothing sent and no 300.
#[test]
fn past_the_ticker_limit_a_stream_gets_101_and_waits_for_a_line() {
    let (client, rx, shared) = test_client();
    shared.reference.set_snapshot_rate_limit(1);
    let engine = line_engine(rx);
    client.req_mkt_data(1, &aapl_stk(), "", false, false).unwrap();
    client.req_mkt_data(2, &spy_stk(), "", false, false).unwrap();
    client.req_mkt_data(3, &aapl_stk(), "", false, false).unwrap();
    client.req_mkt_data(4, &eur_usd(), "", false, false).unwrap();
    let mut w = RecordingWrapper::default();
    client.req_mkt_data(4, &eur_usd(), "", false, false).unwrap();
    client.process_msgs(&mut w);
    let errors: Vec<&String> = w.events.iter().filter(|e| e.starts_with("error:")).collect();
    assert_eq!(errors, [
        "error:2:101:Max number of tickers has been reached",
        "error:4:101:Max number of tickers has been reached",
        "error:4:322:Error processing request.-'bQ' : cause - Duplicate ticker id",
    ]);

    client.cancel_mkt_data(4).unwrap();
    client.cancel_mkt_data(1).unwrap();
    client.cancel_mkt_data(3).unwrap();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(!w.events.iter().any(|e| e.starts_with("error:")), "{:?}", w.events);
    assert_eq!(client.core.md_lines_in_use(), 1);
    let seen = engine.join().unwrap();
    assert_eq!(seen, ["subscribe:265598:0", "unsubscribe:10", "subscribe:756733:0"]);
}

// ═══════════════════════════════════════════════════════════════════
//  Unset contract fields (the official API's defaults)
// ═══════════════════════════════════════════════════════════════════

// A contract with no exchange (empty, the official API's default): the
// reference's refusal of each request that needs one, nothing sent.
#[test]
fn requests_of_a_contract_with_no_exchange_are_refused() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let no_exchange = Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), ..Default::default() };
    let order = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "MKT".into(), ..Default::default() };
    client.place_order(1, &no_exchange, &order).unwrap();
    client.req_mkt_data(2, &no_exchange, "", false, false).unwrap();
    client.req_historical_data(3, &no_exchange, "", "1 D", "1 hour", "TRADES", true, 1, false).unwrap();
    client.req_head_time_stamp(4, &no_exchange, "TRADES", true, 1).unwrap();
    assert!(rx.try_recv().is_err(), "nothing sent");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let errors: Vec<&String> = w.events.iter().filter(|e| e.starts_with("error:")).collect();
    assert_eq!(errors, [
        "error:1:321:Error validating request.-'bH' : cause - Missing order exchange",
        "error:2:321:Error validating request.-'bQ' : cause - Please enter exchange",
        "error:3:321:Error validating request.-'bM' : cause - Please enter exchange",
        "error:4:321:Error validating request.-'bN' : cause - Please enter exchange",
    ]);
}

// A contract lookup with no security type (empty, the official API's
// default) and no conId: 321; with no symbol either, the symbol's text
// first. An empty exchange or currency is not refused: the lookup goes
// out without them.
#[test]
fn contract_details_of_an_unset_contract() {
    let (client, rx, shared) = test_client();
    client.req_contract_details(1, &Contract::default()).unwrap();
    client.req_contract_details(2, &Contract { symbol: "AAPL".into(), ..Default::default() }).unwrap();
    assert!(rx.try_recv().is_err(), "nothing sent");
    assert_eq!(shared.reference.drain_historical_errors(), [
        (1, 321, "Error validating request.-'bK' : cause - The symbol or the local-symbol or the security id must be entered".to_string()),
        (2, 321, "Error validating request.-'bK' : cause - Please enter a valid security type".to_string()),
    ]);
    client.req_contract_details(3, &Contract { symbol: "AAPL".into(), sec_type: "STK".into(), ..Default::default() }).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::FetchContractDetails { req_id: 3, exchange, currency, .. } => assert_eq!((exchange.as_str(), currency.as_str()), ("", "")),
        other => panic!("expected FetchContractDetails, got {:?}", other),
    }
    // A conId alone, or an issuer id alone, needs no security type.
    client.req_contract_details(4, &Contract { con_id: 265598, ..Default::default() }).unwrap();
    client.req_contract_details(5, &Contract { issuer_id: "e1234567".into(), ..Default::default() }).unwrap();
    assert_eq!(rx.try_iter().count(), 2);
    assert!(shared.reference.drain_historical_errors().is_empty());
}

// An order with the official API's defaults beside its type, side and
// quantity goes out as before: no price, no minimum quantity, no cash
// quantity, DAY; openOrder shows the reference's values of the unset
// fields (lmtPrice and auxPrice 0, volatilityType and referencePriceType 0,
// dontUseAutoPriceForHedge true, DAY).
#[test]
fn an_order_with_unset_fields_goes_out_and_shows_as_the_reference() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "MKT".into(), ..Default::default() };
    client.place_order(5, &spy(), &order).unwrap();
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::Order(OrderRequest::SubmitMarket { order_id: 5, qty: 1, .. })));
    let lmt = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 1.0, ..Default::default() };
    client.place_order(6, &spy(), &lmt).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitLimit { order_id: 6, price, .. }) => assert_eq!(price, PRICE_SCALE),
        other => panic!("expected SubmitLimit, got {:?}", other),
    }
    let view = client.core.order_view(5, &client.shared, "Submitted").expect("tracked");
    let o = &view.order;
    assert_eq!((o.lmt_price, o.aux_price, o.tif.as_str()), (0.0, 0.0, "DAY"));
    assert_eq!((o.volatility_type, o.reference_price_type, o.dont_use_auto_price_for_hedge), (0, 0, true));
    assert_eq!((o.min_qty, o.trailing_percent, o.cash_qty, o.filled_quantity), (i32::MAX, f64::MAX, f64::MAX, 0.0));
}

// ibx#420: reqManagedAccts answers every account of the logon's account
// list, in logon order, comma separated (the protobuf form of the
// reference's MANAGED_ACCTS at server version 214); the logon account
// when the logon had no list.
#[test]
fn managed_accounts_are_the_logon_account_list() {
    #[derive(Default)]
    struct Accounts(Vec<String>);
    impl Wrapper for Accounts {
        fn managed_accounts(&mut self, accounts_list: &str) { self.0.push(accounts_list.to_string()); }
    }
    let (client, _rx, shared) = test_client();
    let mut w = Accounts::default();
    client.req_managed_accts(&mut w);
    shared.reference.set_managed_accounts(crate::control::logon::managed_accounts("DUXXXXXX2/{alias},DUXXXXXX1/{alias}"));
    client.req_managed_accts(&mut w);
    shared.reference.set_managed_accounts(crate::control::logon::managed_accounts("DUXXXXXXX"));
    client.req_managed_accts(&mut w);
    assert_eq!(w.0, ["DU123", "DUXXXXXX2,DUXXXXXX1", "DUXXXXXXX"]);
}

// ibx#421: accounts whose application is not approved (8092): an order
// for one is refused with 10136 and nothing is sent (not on an FA
// session); reqPositions gives 10275 and stops when every account is
// pending, gives it as a warning and goes on when some are;
// reqPositionsMulti for one gives 10275 with its request id;
// reqAccountUpdates gives the warning and goes on.
#[test]
fn pending_accounts_refuse_orders_and_positions() {
    #[derive(Default)]
    struct Errors(Vec<(i64, i64, String)>);
    impl Wrapper for Errors {
        fn error(&mut self, req_id: i64, code: i64, msg: &str, _json: &str) { self.0.push((req_id, code, msg.to_string())); }
    }
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    shared.reference.set_pending_accounts(vec!["DU123".into()]);
    let order = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 100.0, ..Default::default() };
    client.place_order(5, &spy(), &order).unwrap();
    assert!(rx.try_recv().is_err(), "nothing sent");
    assert_eq!(shared.orders.drain_order_errors(),
        [(5, 10136, "Cannot submit trades until the application is finished and approved".to_string())]);
    shared.reference.set_fa_session(true);
    client.place_order(6, &spy(), &order).unwrap();
    assert!(rx.try_recv().is_ok(), "an FA session is not checked");
    shared.reference.set_fa_session(false);

    let text = |a: &str| format!("Positions info is not available for account(s): {} until the application is finished and approved.", a);
    shared.reference.set_managed_accounts(vec!["DU123".into()]);
    client.req_positions(&mut Errors::default());
    assert_eq!(shared.orders.drain_order_errors(), [(-1, 10275, text("DU123"))]);
    assert!(!client.core.positions_sub.lock().unwrap().is_some(), "every account pending: the request stops");

    shared.reference.set_managed_accounts(vec!["DU123".into(), "DU456".into()]);
    client.req_positions(&mut Errors::default());
    assert_eq!(shared.orders.drain_order_errors(), [(-1, 10275, text("DU123"))]);
    assert!(client.core.positions_sub.lock().unwrap().is_some(), "some pending: a warning, the request goes on");
    client.cancel_positions();

    client.req_positions_multi(9, "DU123", "", &mut Errors::default());
    assert_eq!(shared.orders.drain_order_errors(), [(9, 10275, text("DU123"))]);
    client.req_positions_multi(10, "DU456", "", &mut Errors::default());
    assert!(shared.orders.drain_order_errors().is_empty());

    client.req_account_updates(true, "DU123");
    assert_eq!(shared.orders.drain_order_errors()[0], (-1, 10275, text("DU123")));
}

// ibx#263: an algo time parameter the reference cannot read is refused
// with 10314 and nothing is sent; one with no zone gets the warning 2174
// and the order goes out.
#[test]
fn algo_time_parameters_10314_and_2174() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    shared.reference.add_algo_definitions(include_str!("../../../tests/fixtures/algo/IBALGO-AE-20261002.xml"));
    shared.reference.add_algo_definitions(include_str!("../../../tests/fixtures/algo/IBALGO-AL-STK-20261002.xml"));
    let vwap = |start: &str| Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 100.0,
        algo_strategy: "Vwap".into(),
        algo_params: vec![TagValue { tag: "startTime".into(), value: start.into() }],
        ..Default::default()
    };
    client.place_order(40, &spy(), &vwap("9am")).unwrap();
    assert!(rx.try_recv().is_err(), "nothing sent");
    let errors = shared.orders.drain_order_errors();
    assert_eq!((errors.len(), errors[0].0, errors[0].1), (1, 40, 10314));
    assert!(errors[0].2.starts_with("startTime: The date, time, or time-zone entered is invalid."), "{}", errors[0].2);

    client.place_order(41, &spy(), &vwap("09:00:00")).unwrap();
    assert!(rx.try_recv().is_ok(), "sent");
    let errors = shared.orders.drain_order_errors();
    assert_eq!(errors.iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(41, 2174)]);
}

// ibx#424: the display group requests are answered locally as the
// reference (capture of 07/10/2026): the fixed list, `none` at once for a
// subscription, error 321 with id -1 for a refusal, and nothing for a
// valid update but the lookup of a conId not seen yet.
#[test]
fn display_group_requests_as_the_reference() {
    let (client, rx, shared) = test_client();
    let refusal = |class: &str, cause: &str| format!("error:-1:321:Error validating request.-'{class}' : cause - {cause}");
    let mut w = RecordingWrapper::default();

    client.query_display_groups(1, &mut w);
    client.subscribe_to_group_events(2, 1, &mut w);
    for group in [9, 0, 8, -1] {
        client.subscribe_to_group_events(3, group, &mut w);
    }
    client.subscribe_to_group_events(2, 1, &mut w);
    client.subscribe_to_group_events(7, 1, &mut w);
    assert_eq!(w.events, [
        "display_group_list:1:1|2|3|4|5|6|7".to_string(),
        "display_group_updated:2:none".to_string(),
        refusal("bX", "Invalid window group ID=9"),
        refusal("bX", "Invalid window group ID=0"),
        refusal("bX", "Invalid window group ID=8"),
        refusal("bX", "Invalid window group ID=-1"),
        refusal("bX", "Request with ID=2 was already subscribed."),
        "display_group_updated:7:none".to_string(),
    ]);

    // Updates: the refusals come with the next messages.
    shared.reference.cache_contract(265598, Contract { con_id: 265598, symbol: "AAPL".into(), ..Default::default() });
    client.update_display_group(8, "265598@SMART");
    client.update_display_group(2, "none");
    client.update_display_group(2, "abc@SMART");
    client.update_display_group(2, "0@SMART");
    client.update_display_group(2, "265598@SMART|foo=1");
    client.update_display_group(2, "265598@SMART|action=Foo");
    client.update_display_group(2, "265598@SMART");
    client.update_display_group(2, "265598");
    client.update_display_group(2, "265598@SMART|action=changegroupec");
    assert!(rx.try_recv().is_err(), "a contract seen before is not looked up");
    client.update_display_group(2, "999999999@SMART");
    assert!(matches!(rx.try_recv(), Ok(ControlCommand::DisplayGroupLookup { req_id: 2, con_id: 999999999 })));
    client.unsubscribe_from_group_events(9);
    client.unsubscribe_from_group_events(2);
    client.unsubscribe_from_group_events(2);
    client.update_display_group(2, "265598@SMART");
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let errors: Vec<&String> = w.events.iter().filter(|e| e.starts_with("error:")).collect();
    assert_eq!(errors, [
        &refusal("bZ", "Request with ID=8 failed since request ID wasn't found."),
        &refusal("bZ", "Request with ID=2 failed with invalid contract info=abc@SMART, expected format 'contractId@exchange'"),
        &refusal("bZ", "Request with ID=2 failed with invalid contract info=0@SMART: conid or excahge are missing, expected format 'contractId@exchange'"),
        &refusal("bZ", "Action is unknown. Please check the pattern: conid@exch|param1=value1|...|action=(action)"),
        &refusal("bZ", "Action 'Foo' is unknown"),
        &refusal("bY", "Subscription for Group Events with request ID=9 wasn't found."),
        &refusal("bY", "Subscription for Group Events with request ID=2 wasn't found."),
        &refusal("bZ", "Request with ID=2 failed since request ID wasn't found."),
    ]);
    assert!(!w.events.iter().any(|e| e.starts_with("display_group")), "{:?}", w.events);
}
