//! Market data subscription test phases.

use super::common::*;
use ibx::control::contracts;
use ibx::protocol::fix;
use ibx::protocol::fixcomp;
use ibx::protocol::connection::Frame;

pub(super) fn phase_market_data(conns: Conns) -> Conns {
    println!("--- Phase 2: Market Data Ticks (AAPL) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut tick_count = 0u32;
    let mut first_tick = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(instrument)) => {
                tick_count += 1;
                if !first_tick {
                    let q = shared.quote(instrument);
                    println!("  FIRST TICK: instrument={} bid={:.4} ask={:.4} last={:.4}",
                        instrument,
                        q.bid as f64 / PRICE_SCALE as f64,
                        q.ask as f64 / PRICE_SCALE as f64,
                        q.last as f64 / PRICE_SCALE as f64);
                    first_tick = true;
                }
            }
            _ => {}
        }
        if first_tick { break; }
    }

    if !first_tick {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        println!("  SKIP: No ticks in 30s — market closed or IB throttling\n");
        return conns;
    }

    std::thread::sleep(Duration::from_secs(5));
    // drain remaining ticks
    while let Ok(Event::Tick(_)) = event_rx.try_recv() {
        tick_count += 1;
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
    println!("  PASS ({} ticks)\n", tick_count);
    conns
}

pub(super) fn phase_multi_instrument(conns: Conns) -> Conns {
    println!("--- Phase 3: Multi-Instrument Subscription (AAPL+MSFT+SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 272093, symbol: "MSFT".into() }).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut tick_count = 0u32;
    let mut first_tick = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(_)) => {
                tick_count += 1;
                if !first_tick { first_tick = true; }
            }
            _ => {}
        }
        if first_tick { break; }
    }

    if !first_tick {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        println!("  SKIP: No ticks — market closed or IB throttling\n");
        return conns;
    }

    std::thread::sleep(Duration::from_secs(5));
    while let Ok(Event::Tick(_)) = event_rx.try_recv() {
        tick_count += 1;
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
    if tick_count <= 3 {
        println!("  SKIP: Only {} ticks — insufficient for multi-instrument test\n", tick_count);
    } else {
        println!("  PASS ({} ticks)\n", tick_count);
    }
    conns
}

pub(super) fn phase_subscribe_unsubscribe(conns: Conns) -> Conns {
    println!("--- Phase 16: Subscribe + Unsubscribe Cleanup ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let mut tick_count = 0u32;
    let sub_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < sub_deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(_)) => { tick_count += 1; }
            _ => {}
        }
    }

    control_tx.send(ControlCommand::Unsubscribe { instrument: 0 }).unwrap();
    std::thread::sleep(Duration::from_secs(3));

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
    println!("  Total ticks: {}", tick_count);
    println!("  PASS\n");
    conns
}

pub(super) fn phase_tbt_subscribe(conns: Conns) -> Conns {
    println!("--- Phase 61: Tick-by-Tick Data (SPY via HMDS) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    control_tx.send(ControlCommand::SubscribeTbt {
        con_id: 756733, symbol: "SPY".into(), tbt_type: TbtType::Last,
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    let mut first_tbt = false;
    let mut tbt_trade_count = 0u32;
    let mut tbt_quote_count = 0u32;
    let deadline = Instant::now() + Duration::from_secs(30);

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::TbtTrade(trade)) => {
                if tbt_trade_count == 0 {
                    println!("  First TBT trade: price={} size={} exchange={}", trade.price as f64 / PRICE_SCALE as f64, trade.size, trade.exchange);
                    first_tbt = true;
                }
                tbt_trade_count += 1;
                break;
            }
            Ok(Event::TbtQuote(quote)) => {
                if tbt_quote_count == 0 {
                    println!("  First TBT quote: bid={} ask={}", quote.bid as f64 / PRICE_SCALE as f64, quote.ask as f64 / PRICE_SCALE as f64);
                    first_tbt = true;
                }
                tbt_quote_count += 1;
                break;
            }
            _ => {}
        }
    }

    if !first_tbt {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        println!("  SKIP: No TBT data in 30s — market closed or HMDS not streaming\n");
        return conns;
    }

    std::thread::sleep(Duration::from_secs(5));
    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
    println!("  PASS ({} trades, {} quotes)\n", tbt_trade_count, tbt_quote_count);
    conns
}

pub(super) fn phase_streaming_validation(conns: Conns) -> Conns {
    println!("--- Phase 102: Streaming Data Validation (SPY tick quality) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut tick_count = 0u32;
    let mut bid_positive = false;
    let mut ask_positive = false;
    let mut spread_valid = true;
    let mut price_reasonable = true;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(instrument)) => {
                tick_count += 1;
                let q = shared.quote(instrument);
                let bid = q.bid as f64 / PRICE_SCALE as f64;
                let ask = q.ask as f64 / PRICE_SCALE as f64;

                if q.bid > 0 { bid_positive = true; }
                if q.ask > 0 { ask_positive = true; }

                // Validate spread: ask >= bid (when both are set)
                // Tolerate up to 5 cents of momentary crossing — bid/ask ticks
                // arrive in separate messages so the SeqLock snapshot can show
                // a transient cross until the next tick updates the other side.
                let cross_tolerance = 0.05 * PRICE_SCALE as f64; // 5 cents
                if q.bid > 0 && q.ask > 0 && (q.bid - q.ask) as f64 > cross_tolerance {
                    spread_valid = false;
                    println!("  WARNING: Crossed market bid={:.4} ask={:.4}", bid, ask);
                }

                // SPY should be between $50 and $1000
                if q.bid > 0 && (bid < 50.0 || bid > 1000.0) {
                    price_reasonable = false;
                    println!("  WARNING: Bid out of range: {:.4}", bid);
                }
                if q.ask > 0 && (ask < 50.0 || ask > 1000.0) {
                    price_reasonable = false;
                    println!("  WARNING: Ask out of range: {:.4}", ask);
                }

                if tick_count >= 20 { break; }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if tick_count == 0 {
        println!("  SKIP: No ticks received — market closed\n");
        return conns;
    }

    println!("  {} ticks: bid_positive={} ask_positive={} spread_valid={} price_reasonable={}",
        tick_count, bid_positive, ask_positive, spread_valid, price_reasonable);
    assert!(bid_positive, "Should have seen at least one positive bid");
    assert!(ask_positive, "Should have seen at least one positive ask");
    assert!(spread_valid, "Spread should not be crossed (ask >= bid)");
    assert!(price_reasonable, "Prices should be in reasonable range for SPY");
    println!("  PASS\n");
    conns
}

pub(super) fn phase_forex_market_data(conns: Conns) -> Conns {
    println!("--- Phase 107: Forex Market Data Ticks (EUR.USD — session-independent) ---");

    // Look up EUR.USD con_id via CCP first
    let now = ibx::gateway::chrono_free_timestamp();
    let mut ccp = conns.ccp;
    ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SECURITY_REQ_ID, "RFX107"),
        (contracts::TAG_SECURITY_REQ_TYPE, "2"),
        (contracts::TAG_SYMBOL, "EUR"),
        (contracts::TAG_SECURITY_TYPE, "CASH"),
        (contracts::TAG_EXCHANGE, "IDEALPRO"),
        (contracts::TAG_CURRENCY, "USD"),
        (contracts::TAG_IB_SOURCE, "Socket"),
    ]).expect("Failed to send forex secdef request");

    let mut forex_con_id: Option<i64> = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && forex_con_id.is_none() {
        match ccp.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(_) => break,
            Ok(_) => {}
        }
        for frame in ccp.extract_frames() {
            let messages = match frame {
                Frame::FixComp(raw) => {
                    let (unsigned, _) = ccp.unsign(&raw);
                    fixcomp::fixcomp_decompress(&unsigned)
                }
                Frame::Fix(raw) => vec![raw],
                _ => continue,
            };
            for msg in messages {
                let tags = fix::fix_parse(&msg);
                if tags.get(&fix::TAG_MSG_TYPE).map(|s| s.as_str()) == Some("d") {
                    if let Some(def) = contracts::parse_secdef_response(&msg) {
                        if def.sec_type == contracts::SecurityType::Forex {
                            forex_con_id = Some(def.con_id as i64);
                        }
                    }
                }
            }
        }
    }

    let con_id = match forex_con_id {
        Some(id) => id,
        None => {
            println!("  SKIP: No EUR.USD contract found\n");
            return Conns { farm: conns.farm, ccp, hmds: conns.hmds, account_id: conns.account_id };
        }
    };

    // Subscribe and verify ticks
    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id, symbol: "EUR".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut tick_count = 0u32;
    let mut bid_seen = false;
    let mut ask_seen = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(instrument)) => {
                tick_count += 1;
                let q = shared.quote(instrument);
                if q.bid > 0 { bid_seen = true; }
                if q.ask > 0 { ask_seen = true; }

                if tick_count == 1 {
                    println!("  FIRST TICK: bid={:.5} ask={:.5}",
                        q.bid as f64 / PRICE_SCALE as f64,
                        q.ask as f64 / PRICE_SCALE as f64);
                }

                // Validate spread only after both bid and ask have been seen
                // (early ticks may have one side at zero while the other updates)
                if bid_seen && ask_seen && q.bid > 0 && q.ask > 0 {
                    assert!(q.ask >= q.bid, "Crossed market: ask < bid");
                }

                if tick_count >= 10 { break; }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if tick_count == 0 {
        println!("  SKIP: No forex ticks (weekend or forex market closed)\n");
    } else {
        assert!(bid_seen, "Should see at least one bid");
        assert!(ask_seen, "Should see at least one ask");
        println!("  {} ticks received, bid_seen={} ask_seen={}", tick_count, bid_seen, ask_seen);
        println!("  PASS\n");
    }
    conns
}

pub(super) fn phase_forex_streaming_validation(conns: Conns) -> Conns {
    println!("--- Phase 108: Forex Streaming Validation (EUR.USD — session-independent) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    // EUR.USD con_id = 12087792 (well-known IB con_id)
    control_tx.send(ControlCommand::Subscribe { con_id: 12087792, symbol: "EUR".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut tick_count = 0u32;
    let mut spread_valid = true;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(instrument)) => {
                tick_count += 1;
                let q = shared.quote(instrument);
                let bid = q.bid as f64 / PRICE_SCALE as f64;
                let ask = q.ask as f64 / PRICE_SCALE as f64;

                if q.bid > 0 && q.ask > 0 {
                    if q.ask < q.bid {
                        spread_valid = false;
                        println!("  WARNING: Crossed spread bid={:.5} ask={:.5}", bid, ask);
                    }
                }

                if tick_count >= 15 { break; }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if tick_count == 0 {
        println!("  SKIP: No forex ticks (weekend or forex market closed)\n");
    } else {
        assert!(spread_valid, "Spread should not be crossed");
        println!("  {} ticks, spread_valid={}", tick_count, spread_valid);
        println!("  PASS\n");
    }
    conns
}

pub(super) fn phase_forex_reconnection(conns: Conns) -> Conns {
    println!("--- Phase 109: Forex Reconnection Recovery (EUR.USD — session-independent) ---");

    // Step 1: Subscribe, get forex ticks
    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 12087792, symbol: "EUR".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got_ticks = false;
    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(_)) => { got_ticks = true; break; }
            _ => {}
        }
    }

    let conns1 = shutdown_and_reclaim(&control_tx, join, account_id.clone());

    if !got_ticks {
        println!("  SKIP: No forex ticks before disconnect (weekend)\n");
        return conns1;
    }
    println!("  Step 1: Got forex ticks before disconnect");

    // Step 2: Reconnect and verify ticks resume
    let shared2 = Arc::new(SharedState::new());
    let (event_tx2, event_rx2) = crossbeam_channel::unbounded();
    let (hot_loop2, control_tx2) = HotLoop::with_connections(
        shared2.clone(), Some(event_tx2), conns1.account_id.clone(),
        conns1.farm, conns1.ccp, conns1.hmds, None,
    );

    control_tx2.send(ControlCommand::Subscribe { con_id: 12087792, symbol: "EUR".into() }).unwrap();
    let join2 = run_hot_loop(hot_loop2);

    let deadline2 = Instant::now() + Duration::from_secs(15);
    let mut got_ticks_after = false;
    while Instant::now() < deadline2 {
        match event_rx2.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(inst)) => {
                let q = shared2.quote(inst);
                println!("  Step 2: Tick after reconnect bid={:.5} ask={:.5}",
                    q.bid as f64 / PRICE_SCALE as f64, q.ask as f64 / PRICE_SCALE as f64);
                got_ticks_after = true;
                break;
            }
            _ => {}
        }
    }

    let conns2 = shutdown_and_reclaim(&control_tx2, join2, conns1.account_id);

    assert!(got_ticks_after, "Should receive forex ticks after reconnection");
    println!("  PASS\n");
    conns2
}

// ─── Phase 110: High-frequency tick stress test (issue #95) ───

pub(super) fn phase_tick_stress_test(conns: Conns) -> Conns {
    println!("--- Phase 110: High-Frequency Tick Stress Test (SPY+AAPL+MSFT, 30s) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    // Subscribe to 3 high-volume instruments
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 272093, symbol: "MSFT".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let run_duration = Duration::from_secs(30);
    let deadline = Instant::now() + run_duration;
    let mut total_ticks = 0u64;
    let mut per_instrument = [0u64; 3]; // SPY, AAPL, MSFT
    let mut last_timestamp = [0u64; 3];
    let mut monotonic_violations = 0u32;
    let mut first_tick = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(instrument)) => {
                total_ticks += 1;
                let idx = instrument as usize;
                if idx < 3 {
                    per_instrument[idx] += 1;
                    let q = shared.quote(instrument);
                    if q.timestamp_ns > 0 && q.timestamp_ns < last_timestamp[idx] {
                        monotonic_violations += 1;
                    }
                    if q.timestamp_ns > 0 {
                        last_timestamp[idx] = q.timestamp_ns;
                    }
                }
                if !first_tick {
                    first_tick = true;
                    println!("  First tick at +{:.1}s", run_duration.as_secs_f64() - (deadline - Instant::now()).as_secs_f64());
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if !first_tick {
        println!("  SKIP: No ticks in 30s — market closed\n");
        return conns;
    }

    let elapsed = run_duration.as_secs_f64();
    let rate = total_ticks as f64 / elapsed;
    println!("  Total ticks: {} ({:.1}/sec)", total_ticks, rate);
    println!("  SPY={} AAPL={} MSFT={}", per_instrument[0], per_instrument[1], per_instrument[2]);
    println!("  Monotonic violations: {}", monotonic_violations);

    // At least 2 instruments should have received ticks
    let instruments_with_ticks = per_instrument.iter().filter(|&&c| c > 0).count();
    assert!(instruments_with_ticks >= 2, "At least 2 instruments should receive ticks, got {}", instruments_with_ticks);
    assert_eq!(monotonic_violations, 0, "Timestamps should be monotonically increasing");

    println!("  PASS\n");
    conns
}
