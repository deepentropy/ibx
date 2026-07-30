//! Compatibility tests against IB paper account.
//!
//! Requires IB_USERNAME and IB_PASSWORD environment variables.
//! Run with: cargo test --test ib_paper_compat -- --ignored --nocapture
//!
//! All tests share a single Gateway connection to avoid session throttling.
//! Each phase builds a fresh HotLoop, runs it, then reclaims connections.

mod account;
mod common;
mod connection;
mod contracts;
mod coverage;
mod error_handling;
mod heartbeat;
mod historical;
mod market_data;
mod multi_asset;
mod orders;

use std::time::{Duration, Instant};

use ibx::gateway::Gateway;
use ibx::protocol::connection::Frame;
use ibx::protocol::fix;
use ibx::protocol::fixcomp;

use common::*;

#[test]
fn compat_suite() {
    let _ = tracing_subscriber::fmt::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    let (session, et_min) = market_session();
    // US stocks have ticks during regular hours AND extended hours (pre-market + after-hours)
    let needs_ticks = matches!(session, MarketSession::Regular | MarketSession::PreMarket | MarketSession::AfterHours);
    let needs_moc = needs_ticks && et_min < 945;
    println!("=== Compatibility Suite (session={:?}) ===\n", session);
    let suite_start = Instant::now();

    let start = Instant::now();
    let (mut gw, farm_conn, mut ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    let connect_time = start.elapsed();

    connection::phase_ccp_auth(&gw, hmds_conn.is_some(), connect_time);
    connection::phase_extra_farms(&gw, &config, &mut ccp_conn);

    let mut conns = Conns {
        farm: farm_conn,
        ccp: ccp_conn,
        hmds: hmds_conn,
        account_id: gw.account_id.clone(),
    };

    if needs_ticks {
        println!("--- RAW SUBSCRIBE TEST ---");
        let conn = &mut conns.farm;
        let result = conn.send_fixcomp(&[
            (fix::TAG_MSG_TYPE, "V"),
            (fix::TAG_SENDING_TIME, &ibx::gateway::chrono_free_timestamp()),
            (263, "1"),
            (146, "2"),
            (262, "1"),
            (6008, "756733"),
            (207, "BEST"),
            (167, "CS"),
            (264, "442"),
            (6088, "Socket"),
            (9830, "1"),
            (9839, "1"),
            (262, "2"),
            (6008, "756733"),
            (207, "BEST"),
            (167, "CS"),
            (264, "443"),
            (6088, "Socket"),
            (9830, "1"),
            (9839, "1"),
        ]);
        println!("  subscribe sent: {:?}, seq={}", result, conn.seq);

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut got_data = false;
        while Instant::now() < deadline {
            match conn.try_recv() {
                Ok(0) => {}
                Ok(n) => {
                    println!("  recv {} bytes, total buffered: {}", n, conn.buffered());
                    let frames = conn.extract_frames();
                    println!("  {} frames extracted", frames.len());
                    for frame in &frames {
                        let (raw, label) = match frame {
                            Frame::FixComp(r) => (r, "FIXCOMP"),
                            Frame::Binary(r) => (r, "Binary"),
                            Frame::Fix(r) => (r, "FIX"),
                            Frame::Control(r) => (r, "Control"),
                        };
                        let (unsigned, valid) = conn.unsign(raw);
                        if label == "FIXCOMP" {
                            let inner = fixcomp::fixcomp_decompress(&unsigned).unwrap_or_default();
                            for m in &inner {
                                let preview = String::from_utf8_lossy(&m[..std::cmp::min(150, m.len())]);
                                println!("  {} inner (valid={}): {}", label, valid, preview);
                            }
                        } else {
                            let preview = String::from_utf8_lossy(&unsigned[..std::cmp::min(150, unsigned.len())]);
                            println!("  {} (valid={}): {}", label, valid, preview);
                        }
                        got_data = true;
                    }
                }
                Err(e) => {
                    println!("  recv error: {}", e);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !got_data {
            println!("  NO DATA received in 15s");
        }

        // Raw subscribe kills connections (server may close farm, hmds, and/or CCP).
        // Reconnect everything if CCP died, or just farm+hmds if CCP survived.
        conns = ensure_ccp_alive(conns, &mut gw, &config);
        // If CCP was alive but farm/hmds died, reconnect them individually.
        match ibx::gateway::connect_farm(
            &config.host, "usfarm", &config.username, &config.password, config.paper,
            &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded,
        18,
        ) {
            Ok(c) => { conns.farm = c; println!("  farm reconnected"); }
            Err(e) => { println!("  farm reconnect failed (may already be fresh): {}", e); }
        }
        match ibx::gateway::connect_farm(
            &config.host, "ushmds", &config.username, &config.password, config.paper,
            &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded,
        17,
        ) {
            Ok(c) => { conns.hmds = Some(c); println!("  hmds reconnected"); }
            Err(e) => { println!("  hmds reconnect failed (may already be fresh): {}", e); }
        }
        println!();
    } else {
        println!("--- RAW SUBSCRIBE TEST ---\n  SKIP: {:?} — no ticks expected\n", session);
    }

    conns = account::phase_account_pnl(conns);

    // ── Completed orders, PnL, news bulletins (early — before flaky network phases) ──
    conns = account::phase_completed_orders(conns);
    conns = account::phase_enriched_order_cache(conns);
    conns = account::phase_enriched_open_orders(conns);
    conns = account::phase_enriched_positions(conns);
    if needs_ticks {
        conns = account::phase_enriched_exec_details(conns);
    } else {
        println!("--- Phase 133: Enriched exec_details ---\n  SKIP: {:?} — needs fills\n", session);
    }
    conns = account::phase_pnl_subscription(conns);
    conns = account::phase_pnl_subscribe_command(conns);
    conns = account::phase_news_bulletins(conns);

    conns = contracts::phase_contract_details(conns);
    conns = contracts::phase_contract_details_by_symbol(conns);
    contracts::phase_trading_hours(&mut conns);
    conns = contracts::phase_matching_symbols(conns);
    conns = historical::phase_historical_data(conns, &gw, &config);
    conns = historical::phase_historical_daily_bars(conns, &gw, &config);
    conns = historical::phase_cancel_historical(conns, &gw, &config);
    conns = historical::phase_query_error_surfaces(conns, &gw, &config);
    conns = historical::phase_head_timestamp(conns, &gw, &config);
    conns = historical::phase_scanner_subscription(conns, &gw, &config);
    conns = historical::phase_historical_news(conns, &gw, &config);
    conns = historical::phase_fundamental_data(conns, &gw, &config);
    conns = contracts::phase_market_rule_id(conns);

    if needs_ticks {
        conns = market_data::phase_market_data(conns);
        conns = market_data::phase_multi_instrument(conns);
        conns = account::phase_account_data(conns);
    } else {
        println!("--- Phase 2: Market Data Ticks (AAPL) ---\n  SKIP: {:?} — no ticks expected\n", session);
        println!("--- Phase 3: Multi-Instrument Subscription (AAPL+MSFT+SPY) ---\n  SKIP: {:?} — no ticks expected\n", session);
        println!("--- Phase 4: Account Data Reception ---\n  SKIP: {:?} — needs ticks to trigger\n", session);
    }

    conns = orders::phase_outside_rth(conns);
    conns = orders::phase_outside_rth_stop(conns);
    conns = orders::phase_limit_order(conns);
    conns = orders::phase_stop_order(conns);
    conns = orders::phase_stop_limit_order(conns);
    conns = orders::phase_modify_order(conns);
    conns = orders::phase_modify_qty(conns);
    conns = orders::phase_trailing_stop(conns);
    conns = orders::phase_trailing_stop_limit(conns);
    conns = orders::phase_limit_ioc(conns);
    conns = orders::phase_limit_fok(conns);
    conns = orders::phase_stop_gtc(conns);
    conns = orders::phase_stop_limit_gtc(conns);
    conns = orders::phase_mit_order(conns);
    conns = orders::phase_lit_order(conns);
    conns = orders::phase_bracket_order(conns);
    conns = orders::phase_adaptive_order(conns);
    conns = orders::phase_rel_order(conns);
    conns = orders::phase_limit_opg(conns);
    conns = orders::phase_iceberg_order(conns);
    conns = orders::phase_hidden_order(conns);
    conns = orders::phase_short_sell(conns);
    conns = orders::phase_trailing_stop_pct(conns);
    conns = orders::phase_oca_group(conns);
    conns = orders::phase_mtl_order(conns);
    conns = orders::phase_mkt_prt_order(conns);
    conns = orders::phase_stp_prt_order(conns);
    conns = orders::phase_mid_price_order(conns);
    conns = orders::phase_snap_mkt_order(conns);
    conns = orders::phase_snap_mid_order(conns);
    conns = orders::phase_snap_pri_order(conns);
    conns = orders::phase_peg_mkt_order(conns);
    conns = orders::phase_peg_mid_order(conns);
    conns = orders::phase_discretionary_order(conns);
    conns = orders::phase_sweep_to_fill_order(conns);
    conns = orders::phase_all_or_none_order(conns);
    conns = orders::phase_trigger_method_order(conns);
    conns = orders::phase_price_condition_order(conns);
    conns = orders::phase_time_condition_order(conns);
    conns = orders::phase_volume_condition_order(conns);
    conns = orders::phase_multi_condition_order(conns);
    conns = orders::phase_vwap_order(conns);
    conns = orders::phase_twap_order(conns);
    conns = orders::phase_arrival_px_order(conns);
    conns = orders::phase_close_px_order(conns);
    conns = orders::phase_dark_ice_order(conns);
    conns = orders::phase_pct_vol_order(conns);
    conns = orders::phase_peg_bench_order(conns);
    conns = orders::phase_limit_auc_order(conns);
    conns = orders::phase_mtl_auc_order(conns);
    conns = orders::phase_box_top_order(conns);
    conns = orders::phase_what_if_order(conns);
    conns = orders::phase_cash_qty_order(conns);
    conns = orders::phase_fractional_order(conns);
    conns = orders::phase_adjustable_stop_order(conns);

    if needs_ticks && conns.hmds.is_some() {
        conns = market_data::phase_tbt_subscribe(conns);
    } else {
        println!("--- Phase 61: Tick-by-Tick Data (SPY) ---\n  SKIP: needs ticks+HMDS\n");
    }

    if needs_moc {
        conns = orders::phase_moc_order(conns);
        conns = orders::phase_loc_order(conns);
    } else {
        println!("--- Phase 27: MOC Order (SPY) ---\n  SKIP: {:?} et_min={} — only before 3:45 PM ET\n", session, et_min);
        println!("--- Phase 28: LOC Order (SPY) ---\n  SKIP: {:?} et_min={} — only before 3:45 PM ET\n", session, et_min);
    }

    conns = market_data::phase_subscribe_unsubscribe(conns);
    conns = market_data::phase_market_depth(conns);
    conns = market_data::phase_news_ticks(conns);
    conns = heartbeat::phase_heartbeat_keepalive(conns);
    conns = heartbeat::phase_farm_heartbeat_keepalive(conns);

    if needs_ticks {
        conns = orders::phase_market_order(conns);
        conns = orders::phase_commission(conns);
        conns = orders::phase_bracket_fill_cascade(conns);
        conns = orders::phase_pnl_after_round_trip(conns);
    } else {
        println!("--- Phase 6: Market Order Round-Trip (SPY) ---\n  SKIP: {:?} — needs ticks+fills\n", session);
        println!("--- Phase 17: Commission Tracking (GTC+OutsideRTH fill) ---\n  SKIP: {:?} — needs fills\n", session);
        println!("--- Phase 51: Bracket Fill Cascade (SPY) ---\n  SKIP: {:?} — needs fills\n", session);
        println!("--- Phase 52: PnL After Round Trip (SPY) ---\n  SKIP: {:?} — needs fills\n", session);
    }

    conns = heartbeat::phase_heartbeat_timeout_detection(conns);

    // CCP may have died during long-running fill phases or heartbeat timeout test.
    // Reconnect the full gateway session if needed before CCP-dependent phases.
    conns = ensure_ccp_alive(conns, &mut gw, &config);

    conns = contracts::phase_contract_details_channel(conns);
    conns = orders::phase_cancel_reject(conns);
    conns = historical::phase_historical_ticks(conns, &gw, &config);
    conns = historical::phase_histogram_data(conns, &gw, &config);
    conns = historical::phase_historical_schedule(conns, &gw, &config);
    conns = historical::phase_realtime_bars(conns, &gw, &config);
    conns = historical::phase_news_article(conns, &gw, &config);
    conns = historical::phase_fundamental_data_channel(conns, &gw, &config);
    conns = historical::phase_parallel_historical(conns, &gw, &config);
    conns = historical::phase_scanner_params(conns, &gw, &config);
    if needs_ticks {
        conns = account::phase_position_tracking(conns);
    } else {
        println!("--- Phase 97: Position Tracking (SPY) ---\n  SKIP: {:?} — needs fills\n", session);
    }
    conns = connection::phase_connection_recovery(conns, &gw, &config);
    conns = ensure_ccp_alive(conns, &mut gw, &config);

    // ── New compatibility test phases (issues #83-#93) ──
    conns = multi_asset::phase_forex_order(conns);
    conns = multi_asset::phase_futures_order(conns);
    conns = multi_asset::phase_options_order(conns);
    conns = multi_asset::phase_concurrent_orders(conns);
    if needs_ticks {
        conns = market_data::phase_streaming_validation(conns);
    } else {
        println!("--- Phase 102: Streaming Data Validation (SPY) ---\n  SKIP: {:?} — needs ticks\n", session);
    }
    conns = historical::phase_historical_ohlc_validation(conns, &gw, &config);
    conns = error_handling::phase_ib_error_handling(conns);
    if needs_ticks {
        conns = connection::phase_reconnection_state_recovery(conns, &gw, &config);
    } else {
        println!("--- Phase 105: Reconnection State Recovery ---\n  SKIP: {:?} — needs ticks\n", session);
    }
    conns = account::phase_account_summary(conns);

    // ── New test phases (issues #92-#95) ──
    if needs_ticks {
        conns = market_data::phase_tick_stress_test(conns);
    } else {
        println!("--- Phase 110: Tick Stress Test (SPY+AAPL+MSFT) ---\n  SKIP: {:?} — needs ticks\n", session);
    }
    conns = historical::phase_large_historical_dataset(conns, &gw, &config);
    conns = historical::phase_dst_boundary_historical(conns, &gw, &config);
    conns = orders::phase_rapid_order_dedup(conns);
    conns = error_handling::phase_pacing_violation_recovery(conns);

    // ── Order modification edge cases (gap #4) ──
    conns = orders::phase_modify_price_and_qty(conns);
    conns = orders::phase_double_modify(conns);
    conns = orders::phase_cancel_during_modify(conns);

    // ── Authentication failure (gap #5) ──
    connection::phase_auth_wrong_password(&config);

    // ── P0: Global cancel (emergency kill switch) ──
    conns = orders::phase_global_cancel(conns);

    // ── P0: Cancel filled order (expect graceful handling) ──
    if needs_ticks {
        conns = orders::phase_cancel_filled_order(conns);
    } else {
        println!("--- Phase 124: Cancel Filled Order ---\n  SKIP: {:?} — needs fills\n", session);
    }

    // ── P1: Matching symbols via ControlCommand channel ──
    conns = contracts::phase_matching_symbols_channel(conns);

    // ── P1: TBT unsubscribe lifecycle ──
    if needs_ticks && conns.hmds.is_some() {
        conns = market_data::phase_tbt_unsubscribe(conns);
    } else {
        println!("--- Phase 126: TBT Unsubscribe ---\n  SKIP: needs ticks+HMDS\n");
    }

    // ── P1: Cancel data requests (historical, fundamental, histogram, head timestamp) ──
    conns = historical::phase_cancel_data_requests(conns, &gw, &config);

    // ── P2: TBT + regular quotes dual stream ──
    if needs_ticks && conns.hmds.is_some() {
        conns = market_data::phase_tbt_and_quotes_dual_stream(conns);
    } else {
        println!("--- Phase 128: TBT + Regular Quotes Dual Stream ---\n  SKIP: needs ticks+HMDS\n");
    }

    // ── P2: Concurrent subscribe stress (10 instruments) ──
    if needs_ticks {
        conns = market_data::phase_concurrent_subscribe_stress(conns);
    } else {
        println!("--- Phase 129: Concurrent Subscribe Stress ---\n  SKIP: {:?} — needs ticks\n", session);
    }

    // ── P2: Historical data + live orders coexistence ──
    conns = historical::phase_historical_and_orders(conns, &gw, &config);

    // ── P2: RegisterInstrument via ControlCommand channel ──
    conns = connection::phase_register_instrument_channel(conns);

    // ── P2: UpdateParam smoke test ──
    conns = connection::phase_update_param(conns);
    conns = coverage::phase_endpoint_coverage(conns);

    // ── Session-independent forex fallback phases (issue #91) ──
    // EUR.USD trades ~24h Sun-Fri, so these cover tick reception when US stocks are closed.
    if !needs_ticks {
        conns = market_data::phase_forex_market_data(conns);
        conns = market_data::phase_forex_streaming_validation(conns);
        conns = market_data::phase_forex_reconnection(conns);
    }

    let _conns = connection::phase_graceful_shutdown(conns);

    // Session-dependent phases: 2,3,4,6,17,27,28,51,52,61,97,102,105,110,124,126,128,129 = 18
    // Forex fallback phases cover 3 of those when !needs_ticks (107,108,109)
    // Phase 134 (PnL subscribe/cancel command) is session-independent and always runs.
    let total_phases = 129;
    let skipped = if needs_ticks { 0 } else { 18 };
    let forex_fallback = if needs_ticks { 0 } else { 3 };
    let ran = total_phases - skipped + forex_fallback;
    println!("\n=== {}/{} phases ran ({} skipped, {} forex-fallback, {:?}) in {:.1}s ===",
        ran, total_phases, skipped, forex_fallback, session, suite_start.elapsed().as_secs_f64());
}

/// ibx#186 focused live entry — runs only the QueryError phase so you don't
/// pay the full ~128-phase suite cost just to validate this fix.
#[test]
fn query_error_phase_live() {
    let _ = tracing_subscriber::fmt::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== ibx#186 focused live test ===\n");
    let (mut gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");

    let conns = Conns {
        farm: farm_conn,
        ccp: ccp_conn,
        hmds: hmds_conn,
        account_id: gw.account_id.clone(),
    };

    let conns = historical::phase_query_error_surfaces(conns, &gw, &config);
    let conns = ensure_ccp_alive(conns, &mut gw, &config);
    let _ = connection::phase_graceful_shutdown(conns);
}

/// ibx#191 PR A focused live entry — validates that after a full disconnect,
/// a fresh `Gateway::connect` receives the CCP recovery push (35=8 with
/// 150=0/39=0 per ib-agent#155) and that a subsequent
/// `cancel_order(<prior_session_orderId>)` succeeds.
///
/// Two `Gateway::connect` calls in one process. Paper account skips the IBKey
/// gate so this runs unattended. IB throttles back-to-back logins within ~60s,
/// so the test waits 90s between sessions. Run:
///   cargo test --test ib_paper_compat cross_session_recovery_phase_live -- --ignored --nocapture
#[test]
#[ignore]
fn cross_session_recovery_phase_live() {
    let _ = tracing_subscriber::fmt::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== ibx#191 PR A: cross-session recovery test ===\n");

    // ─── Session A: place resting GTC LMT BUY 1 SPY @ $1 (far below market) ───
    println!("Session A: connecting + placing resting LMT GTC BUY 1 SPY @ $1");
    let (gw_a, farm_a, ccp_a, hmds_a) = Gateway::connect(&config)
        .expect("Session A: Gateway::connect failed");
    let account_id = gw_a.account_id.clone();
    drop(gw_a); // gateway state not needed after sockets are out

    let order_id = common::next_order_id();
    println!("  orderId = {}", order_id);

    let session_a_acked = {
        let shared = std::sync::Arc::new(SharedState::new());
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (mut hot_loop, control_tx) = HotLoop::with_connections(
            shared, Some(event_tx), account_id.clone(),
            farm_a, ccp_a, hmds_a, None,
        );
        let inst_id = hot_loop.context_mut().register_instrument(756733);
        hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

        control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
            order_id, instrument: inst_id, side: Side::Buy, qty: 1,
            price: 1_00_000_000, outside_rth: true,
        })).expect("Session A: send order failed");

        let join = run_hot_loop(hot_loop);

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut acked = false;
        let mut rejected = false;
        while Instant::now() < deadline && !acked && !rejected {
            if let Ok(Event::OrderUpdate(u)) = event_rx.recv_timeout(Duration::from_millis(100)) {
                match u.status {
                    OrderStatus::Submitted | OrderStatus::PreSubmitted => acked = true,
                    OrderStatus::Rejected => rejected = true,
                    _ => {}
                }
            }
        }

        let _ = control_tx.send(ControlCommand::Shutdown);
        let _ = join.join();

        if rejected {
            panic!("Session A: order rejected — cleanup not possible automatically (check TWS for orderId={})", order_id);
        }
        acked
    };
    // Conns A dropped → TCP sockets close → CCP session ends.
    assert!(session_a_acked, "Session A: LMT order never acked within 30s");
    println!("  Session A: orderId={} acked + sockets closed", order_id);

    // IB throttles back-to-back Gateway::connect calls ("Never received data
    // start after auth" if too soon). Wait ~90s before attempting Session B.
    let wait_secs = 90u64;
    println!("  Waiting {}s before Session B to clear IB auth throttle...\n", wait_secs);
    std::thread::sleep(Duration::from_secs(wait_secs));

    // ─── Session B: fresh connect → expect 35=8 recovery push → cancel orderId ───
    println!("Session B: fresh Gateway::connect → expect recovery push for orderId={}", order_id);
    let (gw_b, farm_b, ccp_b, hmds_b) = Gateway::connect(&config)
        .expect("Session B: Gateway::connect failed");
    assert_eq!(account_id, gw_b.account_id, "Account ID changed between sessions");
    drop(gw_b);

    let shared = std::sync::Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(),
        farm_b, ccp_b, hmds_b, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let join = run_hot_loop(hot_loop);

    // CCP delivers the recovery push <1s after session establishment per ib-agent#155.
    // Sleep a few seconds so handle_exec_report has time to populate last_clord.
    std::thread::sleep(Duration::from_secs(3));

    println!("  Session B: sending Cancel(orderId={})", order_id);
    let cancel_sent = Instant::now();
    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id }))
        .expect("Session B: send cancel failed");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut cancelled = false;
    let mut cancel_rejected = false;
    while Instant::now() < deadline && !cancelled && !cancel_rejected {
        if let Ok(Event::OrderUpdate(u)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            match u.status {
                OrderStatus::Cancelled => cancelled = true,
                OrderStatus::Rejected => cancel_rejected = true,
                _ => {}
            }
        }
    }
    let cancel_ms = cancel_sent.elapsed().as_millis();

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    if cancel_rejected {
        panic!("Session B: cancel rejected — recovery push did not populate last_clord for orderId={}", order_id);
    }
    assert!(cancelled,
        "Session B: cancel not confirmed within 30s — recovery push likely not parsed correctly. Cleanup orderId={} via GUI.",
        order_id);

    println!("  Session B: cancel confirmed in {}ms", cancel_ms);
    println!("\n  PASS — cross-session cancel works (ibx#191 PR A validated)\n");
}

/// ibx#191 PR B focused live entry — validates `EClient::cancel_order_by_perm_id`.
/// Places a resting LMT GTC, captures the broker-assigned `permId` from
/// `order_status`-flavored OrderUpdate events, then cancels by permId (not by
/// local orderId) and asserts Cancelled.
/// Run: cargo test --test ib_paper_compat cancel_by_perm_id_phase_live -- --ignored --nocapture
#[test]
#[ignore]
fn cancel_by_perm_id_phase_live() {
    let _ = tracing_subscriber::fmt::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== ibx#191 PR B: cancel_order_by_perm_id ===\n");
    let (gw, farm, ccp, hmds) = Gateway::connect(&config)
        .expect("Gateway::connect failed");
    let account_id = gw.account_id.clone();
    drop(gw);

    let order_id = common::next_order_id();
    println!("  orderId = {}", order_id);

    let shared = std::sync::Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        farm, ccp, hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1,
        price: 1_00_000_000, outside_rth: true,
    })).expect("send order failed");

    let join = run_hot_loop(hot_loop);

    // Wait for OrderUpdate(Submitted/PreSubmitted) to capture permId.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut perm_id: i64 = 0;
    let mut rejected = false;
    while Instant::now() < deadline && perm_id == 0 && !rejected {
        if let Ok(Event::OrderUpdate(u)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            match u.status {
                OrderStatus::Submitted | OrderStatus::PreSubmitted => {
                    if u.perm_id != 0 {
                        perm_id = u.perm_id;
                    }
                }
                OrderStatus::Rejected => rejected = true,
                _ => {}
            }
        }
    }

    if rejected {
        let _ = control_tx.send(ControlCommand::Shutdown);
        let _ = join.join();
        panic!("order rejected — cleanup orderId={} via GUI", order_id);
    }
    assert!(perm_id != 0,
        "permId never surfaced via OrderUpdate within 30s — cleanup orderId={} via GUI",
        order_id);
    println!("  Captured permId={} for orderId={}", perm_id, order_id);

    // Look up the local orderId by permId (mirrors EClient::cancel_order_by_perm_id).
    // collect_open_orders merges shared.orders.order_cache (populated by 35=8 ack)
    // with ClientCore.open_orders.
    let core = ibx::client_core::ClientCore::new();
    let found = core.collect_open_orders(&shared)
        .into_iter()
        .find(|(_, t)| t.order.perm_id == perm_id);
    let resolved_order_id = match found {
        Some((oid, _)) => oid,
        None => {
            let _ = control_tx.send(ControlCommand::Shutdown);
            let _ = join.join();
            panic!("permId {} not found in open orders cache — cleanup orderId={} via GUI",
                perm_id, order_id);
        }
    };
    assert_eq!(resolved_order_id, order_id,
        "permId→orderId lookup resolved to {} but expected {}", resolved_order_id, order_id);
    println!("  Resolved permId={} → orderId={}", perm_id, resolved_order_id);

    println!("  Sending Cancel(orderId={}) via permId-resolved path", resolved_order_id);
    let cancel_sent = Instant::now();
    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: resolved_order_id }))
        .expect("send cancel failed");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut cancelled = false;
    let mut cancel_rejected = false;
    while Instant::now() < deadline && !cancelled && !cancel_rejected {
        if let Ok(Event::OrderUpdate(u)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            match u.status {
                OrderStatus::Cancelled => cancelled = true,
                OrderStatus::Rejected => cancel_rejected = true,
                _ => {}
            }
        }
    }
    let cancel_ms = cancel_sent.elapsed().as_millis();

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    if cancel_rejected {
        panic!("cancel rejected — cleanup orderId={} via GUI", order_id);
    }
    assert!(cancelled,
        "cancel not confirmed within 30s — cleanup orderId={} via GUI",
        order_id);

    println!("  Cancel confirmed in {}ms", cancel_ms);
    println!("\n  PASS — cancel_order_by_perm_id works (ibx#191 PR B validated)\n");
}

/// ibx#224 / ibx#215 focused live entry — validates the `SubmitEx` wire path:
/// a bracket-style child (STP, GTC, parent_id + oca_group) placed against a
/// resting parent. Before ibx#224 those attrs were dropped and the child
/// went live as an unlinked DAY stop.
///
/// Flow: parent LMT GTC BUY 1 SPY @ $1 (never fills) → child STP GTC SELL 1
/// @ $0.50 via `SubmitEx` linked with parent_id + oca_group → both must ack
/// (child must not be rejected) → cancel the parent → the child must cascade
/// to Cancelled without an explicit cancel, which proves the server saw the
/// parent link.
/// Run: cargo test --test ib_paper_compat submit_ex_bracket_child_phase_live -- --ignored --nocapture
#[test]
#[ignore]
fn submit_ex_bracket_child_phase_live() {
    let _ = tracing_subscriber::fmt::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== ibx#224/ibx#215: SubmitEx bracket child ===\n");
    let (gw, farm, ccp, hmds) = Gateway::connect(&config)
        .expect("Gateway::connect failed");
    let account_id = gw.account_id.clone();
    drop(gw);

    let parent_id = common::next_order_id();
    let child_id = common::next_order_id();
    assert_ne!(parent_id, child_id, "order id collision — parent/child tracking would be meaningless");
    println!("  parent orderId = {}, child orderId = {}", parent_id, child_id);

    let shared = std::sync::Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        farm, ccp, hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    // Parent: resting far-below-market entry (proven pattern from the
    // cross-session test).
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: parent_id, instrument: inst_id, side: Side::Buy, qty: 1,
        price: 1_00_000_000, outside_rth: true,
    })).expect("send parent failed");

    // Child: the ibx#224 shape — STP + GTC + parent_id + oca_group. A sell
    // stop at $0.50 can never trigger even if something goes wrong.
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitEx {
        order_id: child_id, instrument: inst_id, side: Side::Sell, qty: 1,
        kind: ibx::types::OrderKind::Stop { stop_price: 50_000_000 },
        tif: b'1', // GTC
        attrs: ibx::types::OrderAttrs {
            parent_id,
            oca_group: parent_id,
            outside_rth: true,
            ..ibx::types::OrderAttrs::default()
        },
    })).expect("send child failed");

    let join = run_hot_loop(hot_loop);

    // Wait for both to ack.
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut parent_acked, mut child_acked) = (false, false);
    let mut rejected: Option<u64> = None;
    while Instant::now() < deadline && !(parent_acked && child_acked) && rejected.is_none() {
        if let Ok(Event::OrderUpdate(u)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            println!("  [update] oid={} status={:?} parentId={}", u.order_id, u.status, u.parent_id);
            match u.status {
                OrderStatus::Submitted | OrderStatus::PreSubmitted => {
                    if u.order_id == parent_id { parent_acked = true; }
                    if u.order_id == child_id { child_acked = true; }
                }
                OrderStatus::Rejected => rejected = Some(u.order_id),
                _ => {}
            }
        }
    }

    if let Some(oid) = rejected {
        // Best-effort cleanup of whatever did rest before failing.
        let _ = control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: parent_id }));
        let _ = control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: child_id }));
        std::thread::sleep(Duration::from_secs(3));
        let _ = control_tx.send(ControlCommand::Shutdown);
        let _ = join.join();
        panic!("order {} rejected — SubmitEx encoding not accepted; check GUI for leftovers", oid);
    }
    assert!(parent_acked, "parent never acked within 30s — cleanup {}/{} via GUI", parent_id, child_id);
    assert!(child_acked, "child never acked within 30s — the SubmitEx child likely vanished; cleanup {} via GUI", parent_id);
    println!("  Both acked. Cancelling parent, expecting child to cascade...");

    // Cancel the parent only. A linked child must cascade to Cancelled.
    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: parent_id }))
        .expect("send parent cancel failed");

    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut parent_cancelled, mut child_cancelled) = (false, false);
    while Instant::now() < deadline && !(parent_cancelled && child_cancelled) {
        if let Ok(Event::OrderUpdate(u)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            println!("  [update] oid={} status={:?}", u.order_id, u.status);
            if u.status == OrderStatus::Cancelled {
                if u.order_id == parent_id { parent_cancelled = true; }
                if u.order_id == child_id { child_cancelled = true; }
            }
        }
    }

    // Safety net: never leave the child resting, even on failure.
    if !child_cancelled {
        let _ = control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: child_id }));
        std::thread::sleep(Duration::from_secs(3));
    }
    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    assert!(parent_cancelled, "parent cancel not confirmed within 30s — check GUI");
    assert!(child_cancelled,
        "child did NOT cascade-cancel with its parent — parent link not honored on the wire (ibx#224)");

    println!("\n  PASS — SubmitEx child linked, held, and cascade-cancelled (ibx#224/ibx#215 validated)\n");
}

/// ibx#216 focused live entry — validates snap-to-tick on the outbound path.
/// Subscribes SPY market data (the subscribe ack carries the tick size the
/// engine stores), then places a resting LMT GTC BUY 1 SPY at the off-grid
/// price $1.001234. The engine must snap it to $1.00 before encoding; the
/// assertion reads the price back from the server-echoed open-order cache.
/// Run: cargo test --test ib_paper_compat snap_to_tick_phase_live -- --ignored --nocapture
#[test]
#[ignore]
fn snap_to_tick_phase_live() {
    let _ = tracing_subscriber::fmt::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== ibx#216: snap-to-tick ===\n");
    let (gw, farm, ccp, hmds) = Gateway::connect(&config)
        .expect("Gateway::connect failed");
    let account_id = gw.account_id.clone();
    drop(gw);

    let order_id = common::next_order_id();
    println!("  orderId = {}", order_id);

    let shared = std::sync::Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        farm, ccp, hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    // Subscribe first: the subscribe ack is what populates the engine's
    // per-instrument tick size. Without it the snap is a no-op.
    control_tx.send(ControlCommand::Subscribe {
        con_id: 756733, symbol: "SPY".into(), exchange: String::new(),
        sec_type: String::new(), last_trade_date: String::new(), strike: 0.0,
        right: String::new(), multiplier: String::new(), mode_9887: 0, reply_tx: None,
    }).expect("send subscribe failed");

    let join = run_hot_loop(hot_loop);

    // Give the subscribe ack time to land so the tick size is known.
    std::thread::sleep(Duration::from_secs(5));

    // Off-grid on a $0.01 grid: must go out as $1.00.
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1,
        price: 1_00_123_400, outside_rth: true,
    })).expect("send order failed");

    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut acked, mut rejected) = (false, false);
    while Instant::now() < deadline && !acked && !rejected {
        if let Ok(Event::OrderUpdate(u)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            println!("  [update] oid={} status={:?}", u.order_id, u.status);
            match u.status {
                OrderStatus::Submitted | OrderStatus::PreSubmitted => acked = true,
                OrderStatus::Rejected => rejected = true,
                _ => {}
            }
        }
    }
    assert!(!rejected, "off-grid price was rejected — snap-to-tick did not apply (ibx#216)");
    assert!(acked, "order never acked within 30s — cleanup orderId={} via GUI", order_id);

    // Read the price back from the server-echoed open-order cache.
    std::thread::sleep(Duration::from_secs(2));
    let core = ibx::client_core::ClientCore::new();
    let echoed = core.collect_open_orders(&shared)
        .into_iter()
        .find(|(oid, _)| *oid == order_id)
        .map(|(_, t)| t.order.lmt_price);
    println!("  server-echoed lmt_price: {:?}", echoed);

    // Cleanup before asserting on the echo.
    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id }))
        .expect("send cancel failed");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut cancelled = false;
    while Instant::now() < deadline && !cancelled {
        if let Ok(Event::OrderUpdate(u)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            if u.order_id == order_id && u.status == OrderStatus::Cancelled { cancelled = true; }
        }
    }
    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();
    assert!(cancelled, "cancel not confirmed within 30s — cleanup orderId={} via GUI", order_id);

    let echoed = echoed.expect("order missing from the open-order cache after ack");
    assert!((echoed - 1.00).abs() < 1e-9,
        "server echoed lmt_price {} — expected the snapped 1.00 (ibx#216)", echoed);

    println!("\n  PASS — off-grid price snapped to the tick grid and accepted (ibx#216 validated)\n");
}

/// ibx#231 / ibx#227 focused live entry — validates that the new deadline
/// sweeps do NOT fire on healthy traffic: a normal contract-details lookup
/// (by con_id and by symbol, including the fan-out) and a normal historical
/// request all still complete with rows/bars followed by their end signals,
/// well inside the sweep deadlines. The timeout paths themselves are
/// unit-tested; this guards the happy path against a premature sweep and
/// the changed end-ordering for by-symbol lookups.
/// Run: cargo test --test ib_paper_compat timeout_sweeps_phase_live -- --ignored --nocapture
#[test]
#[ignore]
fn timeout_sweeps_phase_live() {
    let _ = tracing_subscriber::fmt::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== ibx#231/ibx#227: happy paths under the deadline sweeps ===\n");
    let (gw, farm, ccp, hmds) = Gateway::connect(&config)
        .expect("Gateway::connect failed");
    let account_id = gw.account_id.clone();
    drop(gw);

    let shared = std::sync::Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        farm, ccp, hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());
    let join = run_hot_loop(hot_loop);

    // Helper: wait for rows + end on a req_id, in order.
    let wait_details = |req_id: u32, label: &str| -> (usize, bool, bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let (mut rows, mut end, mut row_after_end) = (0usize, false, false);
        while Instant::now() < deadline && !end {
            match event_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::ContractDetails { req_id: r, .. }) if r == req_id => {
                    if end { row_after_end = true; }
                    rows += 1;
                }
                Ok(Event::ContractDetailsEnd(r)) if r == req_id => end = true,
                _ => {}
            }
        }
        println!("  {}: rows={} end={} row_after_end={}", label, rows, end, row_after_end);
        (rows, end, row_after_end)
    };

    // 1. By con_id — single record.
    control_tx.send(ControlCommand::FetchContractDetails {
        req_id: 6001, con_id: 756733, symbol: String::new(),
        sec_type: String::new(), exchange: String::new(), currency: String::new(), filters: Default::default(),
    }).expect("send details by con_id failed");
    let (rows, end, _) = wait_details(6001, "by-conId SPY");
    assert!(rows >= 1, "by-conId lookup returned no rows");
    assert!(end, "by-conId end never fired — sweep may have eaten the reply (ibx#227)");

    // 2. By symbol — exercises the fan-out counter and the deferred-end path.
    control_tx.send(ControlCommand::FetchContractDetails {
        req_id: 6002, con_id: 0, symbol: "AAPL".into(),
        sec_type: "STK".into(), exchange: String::new(), currency: "USD".into(),
        filters: Default::default(),
    }).expect("send details by symbol failed");
    let (rows, end, row_after_end) = wait_details(6002, "by-symbol AAPL fan-out");
    assert!(rows >= 1, "by-symbol lookup returned no rows");
    assert!(end, "by-symbol end never fired within 30s (ibx#227)");
    assert!(!row_after_end, "a row arrived AFTER end — ordering regression (ibx#227)");

    // 3. Historical bars — must complete without tripping the idle sweep.
    control_tx.send(ControlCommand::FetchHistorical {
        req_id: 6003, con_id: 756733, symbol: "SPY".into(),
        end_date_time: String::new(), duration: "5 D".into(), bar_size: "1 day".into(),
        what_to_show: "TRADES".into(), use_rth: true, keep_up_to_date: false,
    }).expect("send historical failed");
    let deadline = Instant::now() + Duration::from_secs(45);
    let (mut bars, mut complete, mut hist_err) = (0usize, false, None::<String>);
    while Instant::now() < deadline && !complete && hist_err.is_none() {
        if let Ok(Event::HistoricalData { req_id: 6003, data }) = event_rx.recv_timeout(Duration::from_millis(100)) {
            bars += data.bars.len();
            complete = data.is_complete;
        }
        for (rid, code, msg) in shared.reference.drain_historical_errors() {
            if rid == 6003 { hist_err = Some(format!("error {}: {}", code, msg)); }
        }
    }
    println!("  historical SPY 5D/1day: bars={} complete={} err={:?}", bars, complete, hist_err);
    assert!(hist_err.is_none(), "healthy historical request errored: {:?} (ibx#231 sweep too eager?)", hist_err);
    assert!(complete && bars >= 3, "historical did not complete (bars={})", bars);

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    println!("\n  PASS — happy paths complete under the deadline sweeps (ibx#231/ibx#227 validated)\n");
}

/// ibx#233 / ibx#228 focused live entry.
/// Part 1 (ibx#233): subscribe SPY, cancel, re-subscribe — the second
/// registration must REUSE the reclaimed slot id, proving unsubscribed
/// contracts no longer consume the instrument table.
/// Part 2 (ibx#228): a symbol search with zero matches must still deliver
/// (an empty answer on the right req_id), and a following search must land
/// on ITS req_id — previously the empty result poisoned the queue and every
/// later reply was misattributed.
/// Run: cargo test --test ib_paper_compat reclaim_and_symbol_search_phase_live -- --ignored --nocapture
#[test]
#[ignore]
fn reclaim_and_symbol_search_phase_live() {
    let _ = tracing_subscriber::fmt::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== ibx#233/ibx#228: slot reclaim + symbol search ===\n");
    let (gw, farm, ccp, hmds) = Gateway::connect(&config)
        .expect("Gateway::connect failed");
    let account_id = gw.account_id.clone();
    drop(gw);

    let shared = std::sync::Arc::new(SharedState::new());
    let (event_tx, _event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        farm, ccp, hmds, None,
    );
    let join = run_hot_loop(hot_loop);

    let subscribe = |req: &str| {
        let (tx, rx) = crossbeam_channel::bounded(1);
        control_tx.send(ControlCommand::Subscribe {
            con_id: 756733, symbol: "SPY".into(), exchange: String::new(),
            sec_type: String::new(), last_trade_date: String::new(), strike: 0.0,
            right: String::new(), multiplier: String::new(), mode_9887: 0,
            reply_tx: Some(tx),
        }).expect("send subscribe failed");
        rx.recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("{}: no registration reply", req))
            .unwrap_or_else(|e| panic!("{}: registration rejected: {}", req, e))
    };

    // ── Part 1: reclaim + reuse ──
    let id1 = subscribe("first subscribe");
    println!("  first subscribe: instrument id {}", id1);
    std::thread::sleep(Duration::from_secs(2));
    control_tx.send(ControlCommand::Unsubscribe { instrument: id1 }).expect("send unsubscribe failed");
    std::thread::sleep(Duration::from_secs(2));
    let id2 = subscribe("re-subscribe");
    println!("  re-subscribe: instrument id {}", id2);
    assert_eq!(id2, id1,
        "reclaimed slot must be reused — the cap would stay cumulative (ibx#233)");

    // ── Part 2: symbol search ──
    control_tx.send(ControlCommand::FetchMatchingSymbols { req_id: 7001, pattern: "ZZZZQQXX".into() })
        .expect("send matching symbols failed");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut empty_delivered = false;
    while Instant::now() < deadline && !empty_delivered {
        for (rid, matches) in shared.reference.drain_matching_symbols() {
            println!("  symbol_samples: req_id={} matches={}", rid, matches.len());
            assert_eq!(rid, 7001, "reply misattributed (ibx#228)");
            assert!(matches.is_empty(), "garbage pattern should have no matches");
            empty_delivered = true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(empty_delivered,
        "empty symbol-search result was never delivered — queue poisoned (ibx#228)");

    control_tx.send(ControlCommand::FetchMatchingSymbols { req_id: 7002, pattern: "AAPL".into() })
        .expect("send matching symbols failed");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut aapl_delivered = false;
    while Instant::now() < deadline && !aapl_delivered {
        for (rid, matches) in shared.reference.drain_matching_symbols() {
            println!("  symbol_samples: req_id={} matches={}", rid, matches.len());
            assert_eq!(rid, 7002, "reply landed on the wrong req_id (ibx#228)");
            assert!(!matches.is_empty(), "AAPL search should match");
            aapl_delivered = true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(aapl_delivered, "AAPL symbol search never delivered");

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    println!("\n  PASS — slot reclaimed and reused; symbol search attributes correctly (ibx#233/ibx#228 validated)\n");
}

/// ibx#158 focused live entry — validates the on-demand RTT sample: send a
/// ping, expect a round-trip measurement to land in shared state.
/// Run: cargo test --test ib_paper_compat rtt_ping_phase_live -- --ignored --nocapture
#[test]
#[ignore]
fn rtt_ping_phase_live() {
    let _ = tracing_subscriber::fmt::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== ibx#158: RTT ping ===
");
    let (gw, farm, ccp, hmds) = Gateway::connect(&config)
        .expect("Gateway::connect failed");
    let account_id = gw.account_id.clone();
    drop(gw);

    let shared = std::sync::Arc::new(SharedState::new());
    let (event_tx, _event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        farm, ccp, hmds, None,
    );
    let join = run_hot_loop(hot_loop);

    assert!(shared.last_ccp_rtt().is_none(), "no sample before any probe");
    control_tx.send(ControlCommand::Ping).expect("send ping failed");

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut rtt = None;
    while Instant::now() < deadline && rtt.is_none() {
        rtt = shared.last_ccp_rtt();
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    let rtt = rtt.expect("RTT sample never arrived within 15s (ibx#158)");
    println!("  measured RTT: {:.2} ms", rtt.as_secs_f64() * 1_000.0);
    assert!(rtt.as_millis() < 10_000, "implausible RTT: {:?}", rtt);

    println!("
  PASS — on-demand RTT sample delivered (ibx#158 validated)
");
}
