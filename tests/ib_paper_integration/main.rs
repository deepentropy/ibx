//! Integration tests against IB paper account.
//!
//! Requires IB_USERNAME and IB_PASSWORD environment variables.
//! Run with: cargo test --test ib_paper_integration -- --ignored --nocapture
//!
//! All tests share a single Gateway connection to avoid ONELOGON throttling.
//! Each phase builds a fresh HotLoop, runs it, then reclaims connections.

mod common;
mod connection;
mod contracts;
mod market_data;
mod historical;
mod account;
mod orders;
mod multi_asset;
mod heartbeat;
mod error_handling;

use std::time::{Duration, Instant};

use ibx::gateway::Gateway;
use ibx::protocol::connection::Frame;
use ibx::protocol::fix;
use ibx::protocol::fixcomp;

use common::*;

#[test]
fn integration_suite() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    let (session, et_min) = market_session();
    let needs_ticks = session == MarketSession::Regular;
    let needs_moc = needs_ticks && et_min < 945;
    println!("=== Integration Suite (session={:?}) ===\n", session);
    let suite_start = Instant::now();

    let start = Instant::now();
    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    let connect_time = start.elapsed();

    connection::phase_ccp_auth(&gw, hmds_conn.is_some(), connect_time);
    connection::phase_extra_farms(&gw, &config);

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
                        };
                        let (unsigned, valid) = conn.unsign(raw);
                        if label == "FIXCOMP" {
                            let inner = fixcomp::fixcomp_decompress(&unsigned);
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
        println!();
    } else {
        println!("--- RAW SUBSCRIBE TEST ---\n  SKIP: {:?} — no ticks expected\n", session);
    }

    conns = account::phase_account_pnl(conns);

    // ── Completed orders, PnL, news bulletins (early — before flaky network phases) ──
    conns = account::phase_completed_orders(conns);
    conns = account::phase_pnl_subscription(conns);
    conns = account::phase_news_bulletins(conns);

    contracts::phase_contract_details(&mut conns);
    contracts::phase_contract_details_by_symbol(&mut conns);
    contracts::phase_trading_hours(&mut conns);
    contracts::phase_matching_symbols(&mut conns);
    conns = historical::phase_historical_data(conns, &gw, &config);
    conns = historical::phase_historical_daily_bars(conns, &gw, &config);
    conns = historical::phase_cancel_historical(conns, &gw, &config);
    conns = historical::phase_head_timestamp(conns, &gw, &config);
    conns = historical::phase_scanner_subscription(conns, &gw, &config);
    conns = historical::phase_historical_news(conns, &gw, &config);
    historical::phase_fundamental_data(&gw, &config);
    contracts::phase_market_rule_id(&mut conns);

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

    // ── New integration test phases (issues #83-#93) ──
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

    // ── Session-independent forex fallback phases (issue #91) ──
    // EUR.USD trades ~24h Sun-Fri, so these cover tick reception when US stocks are closed.
    if !needs_ticks {
        conns = market_data::phase_forex_market_data(conns);
        conns = market_data::phase_forex_streaming_validation(conns);
        conns = market_data::phase_forex_reconnection(conns);
    }

    let _conns = connection::phase_graceful_shutdown(conns);

    // Session-dependent phases: 2,3,4,6,17,27,28,51,52,61,97,102,105,110 = 14
    // Forex fallback phases cover 3 of those when !needs_ticks (107,108,109)
    let total_phases = 110;
    let skipped = if needs_ticks { 0 } else { 14 };
    let forex_fallback = if needs_ticks { 0 } else { 3 };
    let ran = total_phases - skipped + forex_fallback;
    println!("\n=== {}/{} phases ran ({} skipped, {} forex-fallback, {:?}) in {:.1}s ===",
        ran, total_phases, skipped, forex_fallback, session, suite_start.elapsed().as_secs_f64());
}
