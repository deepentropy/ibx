//! Integration tests for the control plane modules.
//!
//! Tests cross-module interactions: contracts ↔ FIX protocol, historical ↔ FIX protocol,
//! account parsing, and full workflows that span multiple control plane components.

use ib_engine::control::account::*;
use ib_engine::control::contracts::*;
use ib_engine::control::historical::*;
use ib_engine::protocol::fix;
use ib_engine::protocol::fixcomp;

// ============================================================
// Contract definition: FIX roundtrip (build request → parse response)
// ============================================================

#[test]
fn contract_request_response_roundtrip() {
    // 1. Build a secdef request by conId
    let req_msg = build_secdef_request_by_conid("R42", 265598, 10);
    let req_tags = fix::fix_parse(&req_msg);

    // Verify request structure
    assert_eq!(req_tags[&fix::TAG_MSG_TYPE], "c");
    assert_eq!(req_tags[&TAG_SECURITY_REQ_ID], "R42");
    assert_eq!(req_tags[&TAG_IB_CON_ID], "265598");

    // 2. Simulate server response (35=d)
    let resp_msg = fix::fix_build(
        &[
            (fix::TAG_MSG_TYPE, "d"),
            (TAG_SECURITY_REQ_ID, "R42"),
            (TAG_SECURITY_RESPONSE_TYPE, "4"),
            (TAG_IB_CON_ID, "265598"),
            (TAG_SYMBOL, "AAPL"),
            (TAG_SECURITY_TYPE, "CS"),
            (TAG_SECURITY_EXCHANGE, "NASDAQ"),
            (TAG_IB_PRIMARY_EXCHANGE, "NASDAQ"),
            (TAG_CURRENCY, "USD"),
            (TAG_LONG_NAME, "APPLE INC"),
            (TAG_IB_MIN_TICK, "0.01"),
            (TAG_IB_VALID_EXCHANGES, "BEST,NYSE,ARCA,BATS"),
        ],
        11,
    );

    // 3. Parse response and verify
    let def = parse_secdef_response(&resp_msg).unwrap();
    assert_eq!(def.con_id, 265598);
    assert_eq!(def.symbol, "AAPL");
    assert_eq!(def.sec_type, SecurityType::Stock);
    assert_eq!(def.primary_exchange, "NASDAQ");
    assert_eq!(def.currency, "USD");
    assert_eq!(def.min_tick, 0.01);

    // BEST→SMART in valid exchanges
    assert!(def.valid_exchanges.contains(&"SMART".to_string()));
    assert!(def.valid_exchanges.contains(&"NYSE".to_string()));

    // 4. Verify request ID matches
    let resp_req_id = secdef_response_req_id(&resp_msg).unwrap();
    assert_eq!(resp_req_id, "R42");
}

#[test]
fn contract_symbol_lookup_roundtrip() {
    let req_msg = build_secdef_request_by_symbol(
        "S1",
        "MSFT",
        SecurityType::Stock,
        "SMART",
        "USD",
        1,
    );
    let tags = fix::fix_parse(&req_msg);
    assert_eq!(tags[&TAG_SYMBOL], "MSFT");
    assert_eq!(tags[&TAG_SECURITY_TYPE], "CS");
    assert_eq!(tags[&TAG_EXCHANGE], "BEST"); // SMART→BEST mapping
    assert_eq!(tags[&TAG_CURRENCY], "USD");
}

#[test]
fn contract_store_multi_instrument_workflow() {
    let mut store = ContractStore::default();

    // Simulate receiving secdef responses for multiple instruments
    let instruments = [
        (265598, "AAPL", "CS", "NASDAQ", "USD", 0.01),
        (272093, "MSFT", "CS", "NASDAQ", "USD", 0.01),
        (756733, "SPY", "CS", "ARCA", "USD", 0.01),
    ];

    for &(con_id, symbol, sec_type, exchange, currency, min_tick) in &instruments {
        store.insert(ContractDefinition {
            con_id,
            symbol: symbol.to_string(),
            sec_type: SecurityType::from_fix(sec_type),
            exchange: exchange.to_string(),
            currency: currency.to_string(),
            min_tick,
            ..Default::default()
        });
    }

    assert_eq!(store.len(), 3);

    // Lookup by conId
    assert_eq!(store.get(265598).unwrap().symbol, "AAPL");
    assert_eq!(store.get(272093).unwrap().symbol, "MSFT");
    assert_eq!(store.get(756733).unwrap().symbol, "SPY");

    // Lookup by symbol
    assert_eq!(
        store.find("AAPL", SecurityType::Stock, "USD").unwrap().con_id,
        265598
    );
    assert!(store.find("GOOG", SecurityType::Stock, "USD").is_none());
}

#[test]
fn option_contract_full_workflow() {
    // Build option contract request by symbol
    let req = build_secdef_request_by_symbol(
        "OPT1",
        "AAPL",
        SecurityType::Option,
        "SMART",
        "USD",
        1,
    );
    let tags = fix::fix_parse(&req);
    assert_eq!(tags[&TAG_SECURITY_TYPE], "OPT");

    // Simulate option response
    let resp = fix::fix_build(
        &[
            (fix::TAG_MSG_TYPE, "d"),
            (TAG_SECURITY_REQ_ID, "OPT1"),
            (TAG_IB_CON_ID, "99999"),
            (TAG_SYMBOL, "AAPL"),
            (TAG_SECURITY_TYPE, "OPT"),
            (TAG_LAST_TRADE_DATE, "20260321"),
            (TAG_STRIKE, "200.0"),
            (TAG_RIGHT, "C"),
            (TAG_MULTIPLIER, "100"),
        ],
        2,
    );

    let def = parse_secdef_response(&resp).unwrap();
    assert_eq!(def.sec_type, SecurityType::Option);
    assert_eq!(def.strike, 200.0);
    assert_eq!(def.right, Some(OptionRight::Call));
    assert_eq!(def.multiplier, 100.0);
    assert_eq!(def.last_trade_date, "20260321");

    // Store and retrieve
    let mut store = ContractStore::default();
    store.insert(def);
    let found = store.get(99999).unwrap();
    assert_eq!(found.strike, 200.0);
}

#[test]
fn secdef_response_pagination() {
    // Simulate multi-message response (responseType 4 = result, 5/6 = last)
    let resp1 = fix::fix_build(
        &[
            (fix::TAG_MSG_TYPE, "d"),
            (TAG_SECURITY_RESPONSE_TYPE, "4"),
            (TAG_IB_CON_ID, "111"),
        ],
        1,
    );
    let resp2 = fix::fix_build(
        &[
            (fix::TAG_MSG_TYPE, "d"),
            (TAG_SECURITY_RESPONSE_TYPE, "4"),
            (TAG_IB_CON_ID, "222"),
        ],
        2,
    );
    let resp_last = fix::fix_build(
        &[
            (fix::TAG_MSG_TYPE, "d"),
            (TAG_SECURITY_RESPONSE_TYPE, "5"),
            (TAG_IB_CON_ID, "333"),
        ],
        3,
    );

    assert!(!secdef_response_is_last(&resp1));
    assert!(!secdef_response_is_last(&resp2));
    assert!(secdef_response_is_last(&resp_last));

    // All three parse successfully
    assert!(parse_secdef_response(&resp1).is_some());
    assert!(parse_secdef_response(&resp2).is_some());
    assert!(parse_secdef_response(&resp_last).is_some());
}

// ============================================================
// Historical data: XML build, parse, and FIX wrapping
// ============================================================

#[test]
fn historical_request_full_workflow() {
    let req = HistoricalRequest {
        query_id: "hd1;;AAPL@SMART TRADES;;1;;true;;0;;I".to_string(),
        con_id: 265598,
        symbol: "AAPL".to_string(),
        sec_type: "CS",
        exchange: "SMART",
        data_type: BarDataType::Trades,
        end_time: "20260228-16:00:00".to_string(),
        duration: "1 d".to_string(),
        bar_size: BarSize::Min5,
        use_rth: true,
    };

    // Build FIX message
    let msg = build_historical_request(&req, 42);
    let tags = fix::fix_parse(&msg);
    assert_eq!(tags[&fix::TAG_MSG_TYPE], "W");

    // Verify XML payload is embedded in tag 6118
    let xml = &tags[&TAG_HISTORICAL_XML];
    assert!(xml.contains("<ListOfQueries>"));
    assert!(xml.contains("<contractID>265598</contractID>"));
    assert!(xml.contains("<exchange>BEST</exchange>")); // SMART→BEST
    assert!(xml.contains("<data>Last</data>"));
    assert!(xml.contains("<step>5 mins</step>"));
    assert!(xml.contains("<timeLength>1 d</timeLength>"));
    assert!(xml.contains("<useRTH>true</useRTH>"));
}

#[test]
fn historical_response_parse_multi_bar() {
    let xml = concat!(
        "<ResultSetBar>",
        "<id>q1</id>",
        "<eoq>true</eoq>",
        "<tz>US/Eastern</tz>",
        "<Events>",
        "<Open><time>20260227-09:30:00</time></Open>",
        "<Bar><time>20260227-09:30:00</time>",
        "<open>150.00</open><close>151.50</close>",
        "<high>152.00</high><low>149.50</low>",
        "<weightedAvg>150.75</weightedAvg>",
        "<volume>500000</volume><count>1500</count></Bar>",
        "<Bar><time>20260227-09:35:00</time>",
        "<open>151.50</open><close>152.25</close>",
        "<high>152.50</high><low>151.00</low>",
        "<weightedAvg>151.88</weightedAvg>",
        "<volume>300000</volume><count>900</count></Bar>",
        "<Bar><time>20260227-09:40:00</time>",
        "<open>152.25</open><close>151.75</close>",
        "<high>152.30</high><low>151.50</low>",
        "<weightedAvg>151.90</weightedAvg>",
        "<volume>200000</volume><count>600</count></Bar>",
        "<Close><time>20260227-16:00:00</time></Close>",
        "</Events>",
        "</ResultSetBar>"
    );

    let resp = parse_bar_response(xml).unwrap();
    assert_eq!(resp.query_id, "q1");
    assert_eq!(resp.timezone, "US/Eastern");
    assert!(resp.is_complete);
    assert_eq!(resp.bars.len(), 3);

    // Verify bar data integrity
    assert_eq!(resp.bars[0].open, 150.0);
    assert_eq!(resp.bars[0].close, 151.5);
    assert_eq!(resp.bars[0].high, 152.0);
    assert_eq!(resp.bars[0].low, 149.5);
    assert_eq!(resp.bars[0].volume, 500000);
    assert_eq!(resp.bars[0].count, 1500);

    // Bars are in chronological order
    assert_eq!(resp.bars[0].time, "20260227-09:30:00");
    assert_eq!(resp.bars[1].time, "20260227-09:35:00");
    assert_eq!(resp.bars[2].time, "20260227-09:40:00");

    // Verify OHLC consistency (high >= open/close >= low)
    for bar in &resp.bars {
        assert!(bar.high >= bar.open, "high < open in bar {}", bar.time);
        assert!(bar.high >= bar.close, "high < close in bar {}", bar.time);
        assert!(bar.low <= bar.open, "low > open in bar {}", bar.time);
        assert!(bar.low <= bar.close, "low > close in bar {}", bar.time);
    }
}

#[test]
fn historical_streaming_subscription_flow() {
    // 1. Send subscription request (with real-time refresh)
    let req = HistoricalRequest {
        query_id: "rt1".to_string(),
        con_id: 265598,
        symbol: "AAPL".to_string(),
        sec_type: "CS",
        exchange: "SMART",
        data_type: BarDataType::Trades,
        end_time: "".to_string(),
        duration: "1800 S".to_string(),
        bar_size: BarSize::Sec5,
        use_rth: false,
    };
    let msg = build_historical_request(&req, 1);
    let tags = fix::fix_parse(&msg);
    assert_eq!(tags[&fix::TAG_MSG_TYPE], "W");

    // 2. Receive ticker ID response
    let ticker_xml = "<ResultSetTickerId><id>rt1</id><tickerId>42</tickerId></ResultSetTickerId>";
    let tid = parse_ticker_id(ticker_xml).unwrap();
    assert_eq!(tid, "42");

    // 3. Cancel subscription
    let cancel_msg = build_cancel_request(&tid, 2);
    let cancel_tags = fix::fix_parse(&cancel_msg);
    assert_eq!(cancel_tags[&fix::TAG_MSG_TYPE], "Z");
    assert!(cancel_tags[&TAG_HISTORICAL_XML].contains("ticker:42"));
}

#[test]
fn historical_incomplete_then_complete() {
    // Simulate multi-message response (eoq=false then eoq=true)
    let xml1 = concat!(
        "<ResultSetBar><id>q1</id><eoq>false</eoq><tz>US/Eastern</tz>",
        "<Events><Bar><time>20260227-09:30:00</time>",
        "<open>150.0</open><close>151.0</close>",
        "<high>152.0</high><low>149.0</low>",
        "<volume>100</volume><count>10</count></Bar></Events>",
        "</ResultSetBar>"
    );
    let xml2 = concat!(
        "<ResultSetBar><id>q1</id><eoq>true</eoq><tz>US/Eastern</tz>",
        "<Events><Bar><time>20260227-09:35:00</time>",
        "<open>151.0</open><close>152.0</close>",
        "<high>153.0</high><low>150.0</low>",
        "<volume>200</volume><count>20</count></Bar></Events>",
        "</ResultSetBar>"
    );

    let resp1 = parse_bar_response(xml1).unwrap();
    let resp2 = parse_bar_response(xml2).unwrap();

    assert!(!resp1.is_complete);
    assert!(resp2.is_complete);
    assert_eq!(resp1.query_id, resp2.query_id);

    // Accumulate bars
    let mut all_bars = resp1.bars;
    all_bars.extend(resp2.bars);
    assert_eq!(all_bars.len(), 2);
}

// ============================================================
// Account: parsing and tracking integration
// ============================================================

#[test]
fn account_full_update_workflow() {
    let mut summary = AccountSummary::default();

    // Simulate a stream of account value updates (as they arrive from CCP)
    let updates = [
        ("AccountCode", "DU12345"),
        ("NetLiquidation", "250000.50"),
        ("TotalCashValue", "50000.00"),
        ("BuyingPower", "500000.00"),
        ("GrossPositionValue", "200000.50"),
        ("MaintMarginReq", "75000.00"),
        ("AvailableFunds", "175000.50"),
        ("ExcessLiquidity", "100000.50"),
        ("Currency", "USD"),
    ];

    for (tag, val) in &updates {
        parse_account_value(tag, val, &mut summary);
    }

    assert_eq!(summary.account_id, "DU12345");
    assert_eq!(summary.net_liquidation, 250000.50);
    assert_eq!(summary.buying_power, 500000.00);
    assert_eq!(summary.currency, "USD");

    // Verify accounting identity: available_funds ≈ net_liq - margin
    let expected_available = summary.net_liquidation - summary.maintenance_margin;
    assert!((summary.available_funds - expected_available).abs() < 0.01);
}

#[test]
fn position_tracker_multi_instrument() {
    let mut tracker = PositionTracker::default();

    // Simulate position updates for multiple instruments
    tracker.update(PositionUpdate {
        account_id: "DU12345".to_string(),
        con_id: 265598,
        symbol: "AAPL".to_string(),
        position: 100.0,
        avg_cost: 150.25,
        market_value: 15025.0,
    });
    tracker.update(PositionUpdate {
        account_id: "DU12345".to_string(),
        con_id: 272093,
        symbol: "MSFT".to_string(),
        position: -50.0,
        avg_cost: 400.00,
        market_value: -20000.0,
    });
    tracker.update(PositionUpdate {
        account_id: "DU12345".to_string(),
        con_id: 756733,
        symbol: "SPY".to_string(),
        position: 200.0,
        avg_cost: 500.00,
        market_value: 100000.0,
    });

    // Verify individual lookups
    assert_eq!(tracker.get(265598).unwrap().position, 100.0);
    assert_eq!(tracker.get(272093).unwrap().position, -50.0);
    assert_eq!(tracker.get(756733).unwrap().position, 200.0);
    assert!(tracker.get(999999).is_none());

    // Verify iteration
    let total_positions: f64 = tracker.all().map(|p| p.position).sum();
    assert_eq!(total_positions, 250.0); // 100 - 50 + 200

    // Simulate position close: AAPL position reduced to 0
    tracker.update(PositionUpdate {
        account_id: "DU12345".to_string(),
        con_id: 265598,
        symbol: "AAPL".to_string(),
        position: 0.0,
        avg_cost: 0.0,
        market_value: 0.0,
    });
    assert_eq!(tracker.get(265598).unwrap().position, 0.0);
}

// ============================================================
// Cross-module: contracts + historical + account
// ============================================================

#[test]
fn contract_lookup_feeds_historical_request() {
    // 1. Get contract definition
    let mut store = ContractStore::default();
    store.insert(ContractDefinition {
        con_id: 265598,
        symbol: "AAPL".to_string(),
        sec_type: SecurityType::Stock,
        exchange: "NASDAQ".to_string(),
        currency: "USD".to_string(),
        min_tick: 0.01,
        ..Default::default()
    });

    // 2. Use contract info to build historical request
    let contract = store.get(265598).unwrap();
    let query_id = format!("hd;;{}@{}", contract.symbol, contract.exchange);
    let con_id = contract.con_id;
    let symbol = contract.symbol.clone();
    let sec_type = contract.sec_type.to_fix();

    let req = HistoricalRequest {
        query_id,
        con_id,
        symbol,
        sec_type,
        exchange: "NASDAQ",
        data_type: BarDataType::Trades,
        end_time: "20260228-16:00:00".to_string(),
        duration: "1 d".to_string(),
        bar_size: BarSize::Min5,
        use_rth: true,
    };

    let msg = build_historical_request(&req, 1);
    let tags = fix::fix_parse(&msg);
    assert!(tags[&TAG_HISTORICAL_XML].contains("<contractID>265598</contractID>"));
    assert!(tags[&TAG_HISTORICAL_XML].contains("<secType>CS</secType>"));
}

#[test]
fn fixcomp_wraps_secdef_response() {
    // Simulate a FIXCOMP-compressed secdef response (as sent over farm connections)
    let inner = fix::fix_build(
        &[
            (fix::TAG_MSG_TYPE, "d"),
            (TAG_IB_CON_ID, "265598"),
            (TAG_SYMBOL, "AAPL"),
            (TAG_SECURITY_TYPE, "CS"),
            (TAG_CURRENCY, "USD"),
        ],
        1,
    );

    // Compress
    let compressed = fixcomp::fixcomp_build(&inner);
    assert!(compressed.starts_with(b"8=FIXCOMP"));

    // Decompress and parse
    let messages = fixcomp::fixcomp_decompress(&compressed);
    assert_eq!(messages.len(), 1);

    let def = parse_secdef_response(&messages[0]).unwrap();
    assert_eq!(def.con_id, 265598);
    assert_eq!(def.symbol, "AAPL");
}

#[test]
fn fix_sign_verify_secdef_request() {
    // Build a secdef request, sign it, verify signature
    let msg = build_secdef_request_by_conid("R1", 265598, 1);
    let mac_key: Vec<u8> = (0..20).collect();
    let iv: Vec<u8> = (0..16).collect();

    let (signed, new_iv) = fix::fix_sign(&msg, &mac_key, &iv);
    let (_, unsign_iv, valid) = fix::fix_unsign(&signed, &mac_key, &iv);

    assert!(valid, "HMAC signature should verify");
    assert_eq!(new_iv, unsign_iv, "IV chain should match");
}
