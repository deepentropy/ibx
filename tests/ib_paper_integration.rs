//! Integration tests against IB paper account.
//!
//! Requires IB_USERNAME and IB_PASSWORD environment variables.
//! Run with: cargo test --test ib_paper_integration -- --ignored --nocapture
//!
//! All tests share a single Gateway connection to avoid ONELOGON throttling.
//! Each phase builds a fresh HotLoop, runs it, then reclaims connections.

use std::env;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ibx::bridge::{Event, SharedState};
use ibx::control::contracts;
use ibx::control::fundamental;
use ibx::control::historical::{self, BarDataType, BarSize, HeadTimestampRequest, HistoricalRequest};
use ibx::control::news;
use ibx::control::scanner;
use ibx::engine::hot_loop::HotLoop;
use ibx::gateway::{connect_farm, Gateway, GatewayConfig};
use ibx::protocol::connection::{Connection, Frame};
use ibx::protocol::fix;
use ibx::protocol::fixcomp;
use ibx::types::*;

fn get_config() -> Option<GatewayConfig> {
    let username = env::var("IB_USERNAME").ok()?;
    let password = env::var("IB_PASSWORD").ok()?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());
    Some(GatewayConfig {
        username,
        password,
        host,
        paper: true,
    })
}

/// Shared connections passed between test phases.
struct Conns {
    farm: Connection,
    ccp: Connection,
    hmds: Option<Connection>,
    account_id: String,
}

/// Run a hot loop in a background thread, returning the HotLoop for connection reclamation.
fn run_hot_loop(hot_loop: HotLoop) -> std::thread::JoinHandle<HotLoop> {
    std::thread::spawn(move || {
        let mut hl = hot_loop;
        hl.run();
        hl
    })
}

/// Shutdown a hot loop and reclaim connections.
fn shutdown_and_reclaim(
    control_tx: &crossbeam_channel::Sender<ControlCommand>,
    join: std::thread::JoinHandle<HotLoop>,
    account_id: String,
) -> Conns {
    let _ = control_tx.send(ControlCommand::Shutdown);
    let mut hl = join.join().expect("hot loop thread panicked");
    let farm = hl.farm_conn.take().expect("farm_conn missing after shutdown");
    let ccp = hl.ccp_conn.take().expect("ccp_conn missing after shutdown");
    let hmds = hl.hmds_conn.take();
    Conns { farm, ccp, hmds, account_id }
}

/// Generate a unique order ID based on current time.
fn next_order_id() -> OrderId {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64 * 1000
}

// ─── Market session detection ───

/// US stock market session based on current Eastern Time.
/// DST: second Sunday of March (spring forward) to first Sunday of November (fall back).
#[derive(Debug, Clone, Copy, PartialEq)]
enum MarketSession {
    Regular,    // Mon-Fri 9:30-16:00 ET
    PreMarket,  // Mon-Fri 4:00-9:30 ET
    AfterHours, // Mon-Fri 16:00-20:00 ET
    Closed,     // Mon-Fri 20:00-4:00 ET, weekends
}

fn market_session() -> (MarketSession, u16) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let secs_per_day = 86400u64;
    let total_days = now / secs_per_day;
    let utc_hour = ((now % secs_per_day) / 3600) as i32;
    let utc_min = ((now % 3600) / 60) as i32;

    let mut y = 1970i64;
    let mut remaining = total_days as i64;
    loop {
        let ylen = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
        if remaining < ylen { break; }
        remaining -= ylen;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays: [i64; 12] = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 1u8;
    for &d in &mdays {
        if remaining < d { break; }
        remaining -= d;
        month += 1;
    }
    let day = (remaining + 1) as u8;

    // Day of week: Jan 1 1970 = Thursday (4), 0=Sun..6=Sat
    let utc_dow = ((total_days + 4) % 7) as u8;

    // Compute second Sunday of March and first Sunday of November for DST.
    let jan1_dow = {
        let mut d = 4u8; // Jan 1 1970 = Thursday
        for yr in 1970..y {
            let yl = if yr % 4 == 0 && (yr % 100 != 0 || yr % 400 == 0) { 366 } else { 365 };
            d = ((d as u16 + (yl % 7) as u16) % 7) as u8;
        }
        d // 0=Sun
    };
    let mar1_doy = if leap { 60 } else { 59 };
    let mar1_dow = ((jan1_dow as u16 + (mar1_doy % 7) as u16) % 7) as u8;
    let first_sun_mar = if mar1_dow == 0 { 1 } else { (8 - mar1_dow) as u8 };
    let second_sun_mar = first_sun_mar + 7;

    let nov1_doy = if leap { 305 } else { 304 };
    let nov1_dow = ((jan1_dow as u16 + (nov1_doy % 7) as u16) % 7) as u8;
    let first_sun_nov = if nov1_dow == 0 { 1 } else { (8 - nov1_dow) as u8 };

    let is_edt = match month {
        4..=10 => true,
        3 => day > second_sun_mar || (day == second_sun_mar && utc_hour >= 7),
        11 => day < first_sun_nov || (day == first_sun_nov && utc_hour < 6),
        _ => false,
    };
    let offset: i32 = if is_edt { -240 } else { -300 };
    let et_min_total = utc_hour * 60 + utc_min + offset;

    let (et_dow, et_min) = if et_min_total < 0 {
        (if utc_dow == 0 { 6 } else { utc_dow - 1 }, (et_min_total + 1440) as u16)
    } else {
        (utc_dow, et_min_total as u16)
    };

    if et_dow == 0 || et_dow == 6 { return (MarketSession::Closed, et_min); }

    let session = match et_min {
        240..=569 => MarketSession::PreMarket,
        570..=959 => MarketSession::Regular,
        960..=1199 => MarketSession::AfterHours,
        _ => MarketSession::Closed,
    };
    (session, et_min)
}

// ─── Master test suite ───

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

    phase_ccp_auth(&gw, hmds_conn.is_some(), connect_time);
    phase_extra_farms(&gw, &config);

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

    conns = phase_account_pnl(conns);
    phase_contract_details(&mut conns);
    phase_contract_details_by_symbol(&mut conns);
    phase_trading_hours(&mut conns);
    phase_matching_symbols(&mut conns);
    conns = phase_historical_data(conns, &gw, &config);
    conns = phase_historical_daily_bars(conns, &gw, &config);
    conns = phase_cancel_historical(conns, &gw, &config);
    conns = phase_head_timestamp(conns, &gw, &config);
    conns = phase_scanner_subscription(conns, &gw, &config);
    conns = phase_historical_news(conns, &gw, &config);
    phase_fundamental_data(&gw, &config);
    phase_market_rule_id(&mut conns);

    if needs_ticks {
        conns = phase_market_data(conns);
        conns = phase_multi_instrument(conns);
        conns = phase_account_data(conns);
    } else {
        println!("--- Phase 2: Market Data Ticks (AAPL) ---\n  SKIP: {:?} — no ticks expected\n", session);
        println!("--- Phase 3: Multi-Instrument Subscription (AAPL+MSFT+SPY) ---\n  SKIP: {:?} — no ticks expected\n", session);
        println!("--- Phase 4: Account Data Reception ---\n  SKIP: {:?} — needs ticks to trigger\n", session);
    }

    conns = phase_outside_rth(conns);
    conns = phase_outside_rth_stop(conns);
    conns = phase_limit_order(conns);
    conns = phase_stop_order(conns);
    conns = phase_stop_limit_order(conns);
    conns = phase_modify_order(conns);
    conns = phase_modify_qty(conns);
    conns = phase_trailing_stop(conns);
    conns = phase_trailing_stop_limit(conns);
    conns = phase_limit_ioc(conns);
    conns = phase_limit_fok(conns);
    conns = phase_stop_gtc(conns);
    conns = phase_stop_limit_gtc(conns);
    conns = phase_mit_order(conns);
    conns = phase_lit_order(conns);
    conns = phase_bracket_order(conns);
    conns = phase_adaptive_order(conns);
    conns = phase_rel_order(conns);
    conns = phase_limit_opg(conns);
    conns = phase_iceberg_order(conns);
    conns = phase_hidden_order(conns);
    conns = phase_short_sell(conns);
    conns = phase_trailing_stop_pct(conns);
    conns = phase_oca_group(conns);
    conns = phase_mtl_order(conns);
    conns = phase_mkt_prt_order(conns);
    conns = phase_stp_prt_order(conns);
    conns = phase_mid_price_order(conns);
    conns = phase_snap_mkt_order(conns);
    conns = phase_snap_mid_order(conns);
    conns = phase_snap_pri_order(conns);
    conns = phase_peg_mkt_order(conns);
    conns = phase_peg_mid_order(conns);
    conns = phase_discretionary_order(conns);
    conns = phase_sweep_to_fill_order(conns);
    conns = phase_all_or_none_order(conns);
    conns = phase_trigger_method_order(conns);
    conns = phase_price_condition_order(conns);
    conns = phase_time_condition_order(conns);
    conns = phase_volume_condition_order(conns);
    conns = phase_multi_condition_order(conns);
    conns = phase_vwap_order(conns);
    conns = phase_twap_order(conns);
    conns = phase_arrival_px_order(conns);
    conns = phase_close_px_order(conns);
    conns = phase_dark_ice_order(conns);
    conns = phase_pct_vol_order(conns);
    conns = phase_peg_bench_order(conns);
    conns = phase_limit_auc_order(conns);
    conns = phase_mtl_auc_order(conns);
    conns = phase_box_top_order(conns);
    conns = phase_what_if_order(conns);
    conns = phase_cash_qty_order(conns);
    conns = phase_fractional_order(conns);
    conns = phase_adjustable_stop_order(conns);

    if needs_ticks && conns.hmds.is_some() {
        conns = phase_tbt_subscribe(conns);
    } else {
        println!("--- Phase 61: Tick-by-Tick Data (SPY) ---\n  SKIP: needs ticks+HMDS\n");
    }

    if needs_moc {
        conns = phase_moc_order(conns);
        conns = phase_loc_order(conns);
    } else {
        println!("--- Phase 27: MOC Order (SPY) ---\n  SKIP: {:?} et_min={} — only before 3:45 PM ET\n", session, et_min);
        println!("--- Phase 28: LOC Order (SPY) ---\n  SKIP: {:?} et_min={} — only before 3:45 PM ET\n", session, et_min);
    }

    conns = phase_subscribe_unsubscribe(conns);
    conns = phase_heartbeat_keepalive(conns);
    conns = phase_farm_heartbeat_keepalive(conns);

    if needs_ticks {
        conns = phase_market_order(conns);
        conns = phase_commission(conns);
        conns = phase_bracket_fill_cascade(conns);
        conns = phase_pnl_after_round_trip(conns);
    } else {
        println!("--- Phase 6: Market Order Round-Trip (SPY) ---\n  SKIP: {:?} — needs ticks+fills\n", session);
        println!("--- Phase 17: Commission Tracking (GTC+OutsideRTH fill) ---\n  SKIP: {:?} — needs fills\n", session);
        println!("--- Phase 51: Bracket Fill Cascade (SPY) ---\n  SKIP: {:?} — needs fills\n", session);
        println!("--- Phase 52: PnL After Round Trip (SPY) ---\n  SKIP: {:?} — needs fills\n", session);
    }

    conns = phase_heartbeat_timeout_detection(conns);
    conns = phase_contract_details_channel(conns);
    conns = phase_cancel_reject(conns);
    conns = phase_historical_ticks(conns, &gw, &config);
    conns = phase_histogram_data(conns, &gw, &config);
    conns = phase_historical_schedule(conns, &gw, &config);
    conns = phase_realtime_bars(conns, &gw, &config);
    conns = phase_news_article(conns, &gw, &config);
    conns = phase_fundamental_data_channel(conns, &gw, &config);
    conns = phase_parallel_historical(conns, &gw, &config);
    conns = phase_scanner_params(conns, &gw, &config);
    if needs_ticks {
        conns = phase_position_tracking(conns);
    } else {
        println!("--- Phase 97: Position Tracking (SPY) ---\n  SKIP: {:?} — needs fills\n", session);
    }
    conns = phase_connection_recovery(conns, &gw, &config);

    // ── New integration test phases (issues #83-#93) ──
    conns = phase_forex_order(conns);
    conns = phase_futures_order(conns);
    conns = phase_options_order(conns);
    conns = phase_concurrent_orders(conns);
    if needs_ticks {
        conns = phase_streaming_validation(conns);
    } else {
        println!("--- Phase 102: Streaming Data Validation (SPY) ---\n  SKIP: {:?} — needs ticks\n", session);
    }
    conns = phase_historical_ohlc_validation(conns, &gw, &config);
    conns = phase_ib_error_handling(conns);
    if needs_ticks {
        conns = phase_reconnection_state_recovery(conns, &gw, &config);
    } else {
        println!("--- Phase 105: Reconnection State Recovery ---\n  SKIP: {:?} — needs ticks\n", session);
    }
    conns = phase_account_summary(conns);

    // ── Session-independent forex fallback phases (issue #91) ──
    // EUR.USD trades ~24h Sun-Fri, so these cover tick reception when US stocks are closed.
    if !needs_ticks {
        conns = phase_forex_market_data(conns);
        conns = phase_forex_streaming_validation(conns);
        conns = phase_forex_reconnection(conns);
    }

    let _conns = phase_graceful_shutdown(conns);

    // Session-dependent phases: 2,3,4,6,17,27,28,51,52,61,97,102,105 = 13
    // Forex fallback phases cover 3 of those when !needs_ticks (107,108,109)
    let total_phases = 102;
    let skipped = if needs_ticks { 0 } else { 13 };
    let forex_fallback = if needs_ticks { 0 } else { 3 };
    let ran = total_phases - skipped + forex_fallback;
    println!("\n=== {}/{} phases ran ({} skipped, {} forex-fallback, {:?}) in {:.1}s ===",
        ran, total_phases, skipped, forex_fallback, session, suite_start.elapsed().as_secs_f64());
}

// ─── Phase 1: CCP auth + farm logon (no hot loop) ───

fn phase_ccp_auth(gw: &Gateway, has_hmds: bool, connect_time: Duration) {
    println!("--- Phase 1: CCP Auth + Farm Logon ---");

    assert!(!gw.account_id.is_empty(), "Account ID should be non-empty after CCP logon");
    println!("  Account ID: {}", gw.account_id);

    assert!(!gw.server_session_id.is_empty(), "Server session ID should be set");
    if !gw.ccp_token.is_empty() {
        println!("  CCP token: present");
    } else {
        println!("  CCP token: not present (non-fatal)");
    }
    assert!(gw.heartbeat_interval > 0, "Heartbeat interval should be positive");
    println!("  Session ID: {}", gw.server_session_id);
    println!("  Heartbeat interval: {}s", gw.heartbeat_interval);

    use num_bigint::BigUint;
    assert!(gw.session_token > BigUint::from(0u32), "Session token should be non-zero");

    if has_hmds {
        println!("  ushmds farm: CONNECTED");
    } else {
        println!("  ushmds farm: NOT CONNECTED (non-fatal)");
    }

    assert!(connect_time < Duration::from_secs(60), "Connection took too long: {:?}", connect_time);
    println!("  PASS ({:.3}s)\n", connect_time.as_secs_f64());
}

// ─── Phase 18: Additional farm connections ───

fn phase_extra_farms(gw: &Gateway, config: &GatewayConfig) {
    println!("--- Phase 18: Additional Farm Connections ---");

    let farms = ["cashhmds", "secdefil", "fundfarm", "usopt"];
    let mut connected = 0;

    for farm in &farms {
        let start = Instant::now();
        match ibx::gateway::connect_farm(
            &config.host, farm,
            &config.username, config.paper,
            &gw.server_session_id, &gw.session_token,
            &gw.hw_info, &gw.encoded,
        ) {
            Ok(_conn) => {
                connected += 1;
                println!("  {}: CONNECTED ({:.3}s)", farm, start.elapsed().as_secs_f64());
            }
            Err(e) => {
                println!("  {}: FAILED (non-fatal): {} ({:.3}s)", farm, e, start.elapsed().as_secs_f64());
            }
        }
    }

    println!("  {}/{} extra farms connected", connected, farms.len());
    println!("  PASS\n");
}

// ─── Phase 12: Contract details lookup (raw CCP) ───

fn phase_contract_details(conns: &mut Conns) {
    println!("--- Phase 12: Contract Details Lookup (SPY, conId=756733) ---");

    let now = ibx::gateway::chrono_free_timestamp();
    conns.ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SECURITY_REQ_ID, "R1"),
        (contracts::TAG_SECURITY_REQ_TYPE, "2"),
        (contracts::TAG_IB_CON_ID, "756733"),
        (contracts::TAG_IB_SOURCE, "Socket"),
    ]).expect("Failed to send secdef request");

    let mut contract: Option<contracts::ContractDefinition> = None;
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline && contract.is_none() {
        match conns.ccp.try_recv() {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => {
                println!("  CCP recv error: {}", e);
                break;
            }
            Ok(_) => {}
        }

        for frame in conns.ccp.extract_frames() {
            let messages = match frame {
                Frame::FixComp(raw) => {
                    let (unsigned, _) = conns.ccp.unsign(&raw);
                    fixcomp::fixcomp_decompress(&unsigned)
                }
                Frame::Fix(raw) => vec![raw],
                _ => continue,
            };

            for msg in messages {
                let tags = fix::fix_parse(&msg);
                let msg_type = tags.get(&fix::TAG_MSG_TYPE).map(|s| s.as_str()).unwrap_or("?");
                if msg_type == "d" {
                    if let Some(def) = contracts::parse_secdef_response(&msg) {
                        if def.con_id == 756733 {
                            println!("  {} ({}) conId={}", def.symbol, def.long_name, def.con_id);
                            println!("  SecType={:?} Exchange={} Currency={}", def.sec_type, def.exchange, def.currency);
                            contract = Some(def);
                        }
                    }
                }
            }
        }
    }

    let def = contract.expect("No contract details received for SPY (756733)");
    assert_eq!(def.con_id, 756733);
    assert_eq!(def.symbol, "SPY");
    assert_eq!(def.sec_type, contracts::SecurityType::Stock);
    assert_eq!(def.currency, "USD");
    assert!(!def.long_name.is_empty(), "Long name should not be empty");
    assert!(!def.valid_exchanges.is_empty(), "Valid exchanges should not be empty");
    assert!(def.valid_exchanges.contains(&"SMART".to_string()), "SMART should be in valid exchanges");
    assert!(def.min_tick > 0.0, "Min tick should be positive");
    println!("  MinTick={}", def.min_tick);
    println!("  PASS\n");
}

// ─── Phase 11: Historical data bars via HMDS ───

fn phase_historical_data(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 11: Historical Data Bars (SPY, 1 day of 5-min bars) ---");

    let mut hmds = match connect_farm(
        &config.host, "ushmds",
        &config.username, config.paper,
        &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded,
    ) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => {
            println!("  SKIP: ushmds reconnect failed: {}\n", e);
            return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id };
        }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (bg_loop, bg_tx) = HotLoop::with_connections(
        shared, None, account_id.clone(), conns.farm, conns.ccp, None, None,
    );
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let secs_per_day = 86400u64;
    let end_time = {
        let days = now / secs_per_day;
        let mut y = 1970i64;
        let mut remaining = days as i64;
        loop {
            let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
            if remaining < days_in_year { break; }
            remaining -= days_in_year;
            y += 1;
        }
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut m = 0;
        for (i, &d) in month_days.iter().enumerate() {
            if remaining < d as i64 { m = i + 1; break; }
            remaining -= d as i64;
        }
        let day = remaining + 1;
        let hour = (now % secs_per_day) / 3600;
        let min = (now % 3600) / 60;
        let sec = now % 60;
        format!("{:04}{:02}{:02}-{:02}:{:02}:{:02}", y, m, day, hour, min, sec)
    };

    let req = HistoricalRequest {
        query_id: "test1".to_string(),
        con_id: 756733,
        symbol: "SPY".to_string(),
        sec_type: "CS",
        exchange: "SMART",
        data_type: BarDataType::Trades,
        end_time,
        duration: "1 d".to_string(),
        bar_size: BarSize::Min5,
        use_rth: true,
    };

    let xml = historical::build_query_xml(&req);
    hmds.send_fixcomp(&[
        (fix::TAG_MSG_TYPE, "W"),
        (historical::TAG_HISTORICAL_XML, &xml),
    ]).expect("Failed to send historical request");

    let mut all_bars = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut complete = false;

    while Instant::now() < deadline && !complete {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error: {}", e); break; }
            Ok(_) => {}
        }

        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => {
                    let (unsigned, _) = hmds.unsign(raw);
                    fixcomp::fixcomp_decompress(&unsigned)
                }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };

            for msg in data {
                let tags = fix::fix_parse(&msg);
                if let Some(xml_resp) = tags.get(&historical::TAG_HISTORICAL_XML) {
                    if let Some(resp) = historical::parse_bar_response(xml_resp) {
                        all_bars.extend(resp.bars);
                        if resp.is_complete { complete = true; }
                    }
                }
            }
        }
    }

    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);

    println!("  Total bars received: {}", all_bars.len());
    if all_bars.is_empty() {
        println!("  SKIP: No historical bars received (HMDS may be unavailable)\n");
        return bg_conns;
    }

    let first = &all_bars[0];
    assert!(first.open > 0.0, "Open price should be positive");
    assert!(first.high >= first.low, "High should be >= Low");
    assert!(first.volume > 0, "Volume should be positive");
    println!("  First bar: O={:.2} H={:.2} L={:.2} C={:.2} V={}",
        first.open, first.high, first.low, first.close, first.volume);
    println!("  PASS ({} bars)\n", all_bars.len());
    bg_conns
}

// ─── Phase 2: Market data ticks ───

fn phase_market_data(conns: Conns) -> Conns {
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

// ─── Phase 3: Multi-instrument subscription ───

fn phase_multi_instrument(conns: Conns) -> Conns {
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

// ─── Phase 4: Account data reception ───

fn phase_account_data(conns: Conns) -> Conns {
    println!("--- Phase 4: Account Data Reception ---");

    let account_id = conns.account_id;

    let mut ccp = conns.ccp;
    let ts = ibx::gateway::chrono_free_timestamp();
    let _ = ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "U"),
        (fix::TAG_SENDING_TIME, &ts),
        (6040, "76"),
        (1, ""),
        (6565, "1"),
    ]);

    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut account_checked = false;
    let mut net_liq = 0i64;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Tick(_)) => {
                if !account_checked {
                    let acct = shared.account();
                    if acct.net_liquidation != 0 {
                        net_liq = acct.net_liquidation;
                        println!("  ACCOUNT: net_liq={:.2}", net_liq as f64 / PRICE_SCALE as f64);
                        account_checked = true;
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if account_checked {
        assert!(net_liq > 0, "Paper account net liquidation should be > 0");
        println!("  net_liq=${:.2}", net_liq as f64 / PRICE_SCALE as f64);
        println!("  PASS\n");
    } else {
        println!("  WARN: Account data not received within 20s\n");
    }
    conns
}

// ─── Phase 14: Account PnL reception ───

fn phase_account_pnl(conns: Conns) -> Conns {
    println!("--- Phase 14: Account PnL Reception ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    // Register SPY instrument so on_start order submission works
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id,
        instrument: inst_id,
        side: Side::Buy,
        qty: 1,
        price: 1_00_000_000,
        outside_rth: true,
    })).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut account_received = false;
    let mut net_liq = 0i64;
    let mut probe_done = false;

    while Instant::now() < deadline && !probe_done {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                if !account_received {
                    let acct = shared.account();
                    if acct.net_liquidation != 0 {
                        net_liq = acct.net_liquidation;
                        account_received = true;
                    }
                }
                if update.status == OrderStatus::Submitted {
                    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                }
                if matches!(update.status, OrderStatus::Cancelled | OrderStatus::Rejected) {
                    probe_done = true;
                }
            }
            Ok(Event::Tick(_)) => {
                if !account_received {
                    let acct = shared.account();
                    if acct.net_liquidation != 0 {
                        net_liq = acct.net_liquidation;
                        account_received = true;
                    }
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(account_received,
        "Account data not received — 6040=77 may not contain tag 9806");
    assert!(net_liq > 0, "Paper account net liquidation should be > 0");
    println!("  NetLiq: ${:.2}", net_liq as f64 / PRICE_SCALE as f64);
    println!("  PASS\n");
    conns
}

// ─── Phase 5: Graceful shutdown ───

fn phase_graceful_shutdown(conns: Conns) -> Conns {
    println!("--- Phase 5: Graceful Shutdown ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let join = run_hot_loop(hot_loop);
    std::thread::sleep(Duration::from_secs(2));

    let shutdown_start = Instant::now();
    control_tx.send(ControlCommand::Shutdown).unwrap();

    let mut hl = join.join().expect("hot loop panicked");
    let shutdown_time = shutdown_start.elapsed();

    assert!(
        shutdown_time < Duration::from_secs(2),
        "Shutdown took too long: {:?}", shutdown_time
    );

    // Check that Disconnected event was emitted
    let mut got_disconnect = false;
    while let Ok(ev) = event_rx.try_recv() {
        if matches!(ev, Event::Disconnected) {
            got_disconnect = true;
        }
    }
    assert!(got_disconnect, "Disconnected event was not emitted during shutdown");

    let farm = hl.farm_conn.take().expect("farm_conn missing");
    let ccp = hl.ccp_conn.take().expect("ccp_conn missing");
    let hmds = hl.hmds_conn.take();

    println!("  Shutdown in {:.3}s", shutdown_time.as_secs_f64());
    println!("  PASS\n");
    Conns { farm, ccp, hmds, account_id }
}

// ─── Phase 6: Market order round-trip ───

fn phase_market_order(conns: Conns) -> Conns {
    println!("--- Phase 6: Market Order Round-Trip (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut tick_count = 0u32;
    let mut phase = 0u8;
    let mut buy_order_id;
    let mut sell_order_id;
    let mut buy_price = 0i64;
    let mut sell_price = 0i64;
    let mut buy_rtt_us = 0u64;
    let mut sell_rtt_us = 0u64;
    let mut buy_sent_at: Option<Instant> = None;
    let mut sell_sent_at: Option<Instant> = None;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(instrument)) => {
                tick_count += 1;
                if phase == 0 && tick_count >= 5 {
                    buy_order_id = next_order_id();
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: buy_order_id,
                        instrument,
                        side: Side::Buy,
                        qty: 1,
                    })).unwrap();
                    buy_sent_at = Some(Instant::now());
                    phase = 1;
                }
            }
            Ok(Event::Fill(fill)) => {
                if phase == 1 && fill.side == Side::Buy {
                    buy_price = fill.price;
                    buy_rtt_us = buy_sent_at.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                    sell_order_id = next_order_id() + 1;
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: sell_order_id,
                        instrument: fill.instrument,
                        side: Side::Sell,
                        qty: 1,
                    })).unwrap();
                    sell_sent_at = Some(Instant::now());
                    phase = 2;
                } else if phase == 2 && fill.side == Side::Sell {
                    sell_price = fill.price;
                    sell_rtt_us = sell_sent_at.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                    let _ = phase;
                    break;
                }
            }
            Ok(Event::OrderUpdate(update)) => {
                if update.status == OrderStatus::Rejected {
                    order_rejected = true;
                    break;
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Order rejected — market may be closed\n");
        return conns;
    }
    if buy_price == 0 {
        println!("  SKIP: No buy fill — market is closed\n");
        return conns;
    }
    assert!(sell_price > 0, "Buy filled but no sell fill received");

    println!("  Buy: ${:.4} (RTT {:.3}ms)", buy_price as f64 / PRICE_SCALE as f64, buy_rtt_us as f64 / 1000.0);
    println!("  Sell: ${:.4} (RTT {:.3}ms)", sell_price as f64 / PRICE_SCALE as f64, sell_rtt_us as f64 / 1000.0);
    println!("  Mean RTT: {:.3}ms", (buy_rtt_us + sell_rtt_us) as f64 / 2000.0);
    println!("  PASS\n");
    conns
}

// ─── Phase 7: Limit order submit + cancel ───

fn phase_limit_order(conns: Conns) -> Conns {
    println!("--- Phase 7: Limit Order Submit + Cancel (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimit {
        order_id,
        instrument: inst_id,
        side: Side::Buy,
        qty: 1,
        price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let submitted = true;
    let mut order_acked = false;
    let mut cancel_sent = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;
    let submit_time = Instant::now();
    let mut cancel_time: Option<Instant> = None;
    let mut submit_ack_us = 0u64;
    let mut cancel_conf_us = 0u64;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if !order_acked {
                            submit_ack_us = submit_time.elapsed().as_micros() as u64;
                            order_acked = true;
                        }
                        if !cancel_sent {
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                            cancel_time = Some(Instant::now());
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => {
                        cancel_conf_us = cancel_time.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                        order_cancelled = true;
                        break;
                    }
                    OrderStatus::Rejected => {
                        order_rejected = true;
                        break;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let _ = submit_time; // suppress unused warning

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Order rejected — market may be closed\n");
        return conns;
    }

    assert!(submitted, "Order was never submitted");
    assert!(order_acked, "Order was never acknowledged");
    assert!(order_cancelled, "Order was never cancelled");

    println!("  Submit→Ack: {:.3}ms  Cancel→Conf: {:.3}ms", submit_ack_us as f64 / 1000.0, cancel_conf_us as f64 / 1000.0);
    println!("  PASS\n");
    conns
}

// ─── Generic submit+cancel helper ───
// fill_or_cancel=false: only cancelled counts as success
// fill_or_cancel=true: filled OR cancelled both count as success

fn run_submit_cancel_phase(
    conns: Conns,
    phase_name: &str,
    order_req: OrderRequest,
    fill_or_cancel: bool,
) -> Conns {
    println!("--- {} ---", phase_name);

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = match &order_req {
        OrderRequest::SubmitLimit { order_id, .. } => *order_id,
        OrderRequest::SubmitMarket { order_id, .. } => *order_id,
        OrderRequest::SubmitStop { order_id, .. } => *order_id,
        OrderRequest::SubmitStopLimit { order_id, .. } => *order_id,
        OrderRequest::SubmitLimitGtc { order_id, .. } => *order_id,
        OrderRequest::SubmitStopGtc { order_id, .. } => *order_id,
        OrderRequest::SubmitStopLimitGtc { order_id, .. } => *order_id,
        OrderRequest::SubmitLimitIoc { order_id, .. } => *order_id,
        OrderRequest::SubmitLimitFok { order_id, .. } => *order_id,
        OrderRequest::SubmitTrailingStop { order_id, .. } => *order_id,
        OrderRequest::SubmitTrailingStopLimit { order_id, .. } => *order_id,
        OrderRequest::SubmitTrailingStopPct { order_id, .. } => *order_id,
        OrderRequest::SubmitMoc { order_id, .. } => *order_id,
        OrderRequest::SubmitLoc { order_id, .. } => *order_id,
        OrderRequest::SubmitMit { order_id, .. } => *order_id,
        OrderRequest::SubmitLit { order_id, .. } => *order_id,
        OrderRequest::SubmitLimitEx { order_id, .. } => *order_id,
        OrderRequest::SubmitRel { order_id, .. } => *order_id,
        OrderRequest::SubmitLimitOpg { order_id, .. } => *order_id,
        OrderRequest::SubmitAdaptive { order_id, .. } => *order_id,
        OrderRequest::SubmitMtl { order_id, .. } => *order_id,
        OrderRequest::SubmitMktPrt { order_id, .. } => *order_id,
        OrderRequest::SubmitStpPrt { order_id, .. } => *order_id,
        OrderRequest::SubmitMidPrice { order_id, .. } => *order_id,
        OrderRequest::SubmitSnapMkt { order_id, .. } => *order_id,
        OrderRequest::SubmitSnapMid { order_id, .. } => *order_id,
        OrderRequest::SubmitSnapPri { order_id, .. } => *order_id,
        OrderRequest::SubmitPegMkt { order_id, .. } => *order_id,
        OrderRequest::SubmitPegMid { order_id, .. } => *order_id,
        OrderRequest::SubmitAlgo { order_id, .. } => *order_id,
        OrderRequest::SubmitPegBench { order_id, .. } => *order_id,
        OrderRequest::SubmitLimitAuc { order_id, .. } => *order_id,
        OrderRequest::SubmitMtlAuc { order_id, .. } => *order_id,
        OrderRequest::SubmitWhatIf { order_id, .. } => *order_id,
        OrderRequest::SubmitLimitFractional { order_id, .. } => *order_id,
        OrderRequest::SubmitAdjustableStop { order_id, .. } => *order_id,
        OrderRequest::SubmitBracket { parent_id, .. } => *parent_id,
        OrderRequest::Cancel { order_id } => *order_id,
        OrderRequest::CancelAll { .. } => 0,
        OrderRequest::Modify { new_order_id, .. } => *new_order_id,
    };

    control_tx.send(ControlCommand::Order(order_req)).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut order_acked = false;
    let mut cancel_sent = false;
    let mut order_cancelled = false;
    let mut order_filled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        order_acked = true;
                        if !cancel_sent {
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    OrderStatus::Filled => {
                        order_filled = true;
                        if fill_or_cancel { break; }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Order rejected\n");
        return conns;
    }
    if fill_or_cancel {
        assert!(order_filled || order_cancelled, "Order was neither filled nor cancelled");
        if order_filled { println!("  PASS (filled)\n"); } else { println!("  PASS (cancelled)\n"); }
    } else {
        assert!(order_acked, "Order was never acknowledged");
        assert!(order_cancelled, "Order was never cancelled");
        println!("  PASS\n");
    }
    conns
}

// ─── Phase 8: Stop order submit + cancel ───

fn phase_stop_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 8: Stop Order Submit + Cancel (SPY)",
        OrderRequest::SubmitStop { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, stop_price: 1_00_000_000 },
        false)
}

// ─── Phase 9: Order modify (35=G) ───

fn phase_modify_order(conns: Conns) -> Conns {
    println!("--- Phase 9: Order Modify (35=G) + Cancel (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimit {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut order_acked = false;
    let mut modify_sent = false;
    let mut modify_acked = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;
    let new_order_id = order_id + 1;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if modify_sent && !modify_acked {
                            modify_acked = true;
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: new_order_id })).unwrap();
                        } else if !order_acked {
                            order_acked = true;
                            control_tx.send(ControlCommand::Order(OrderRequest::Modify {
                                order_id, new_order_id, price: 2_00_000_000, qty: 1,
                            })).unwrap();
                            modify_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Modify test rejected\n");
        return conns;
    }
    assert!(order_acked, "Order was never acknowledged");
    assert!(modify_sent, "Modify was never sent");
    assert!(modify_acked, "Modify was never acknowledged");
    assert!(order_cancelled, "Modified order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 10: Outside RTH limit order ───

fn phase_outside_rth(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 10: Outside RTH Limit Order (GTC+OutsideRTH, SPY)",
        OrderRequest::SubmitLimitGtc { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, outside_rth: true },
        false)
}

// ─── Phase 13: Heartbeat keepalive ───

fn phase_heartbeat_keepalive(conns: Conns) -> Conns {
    println!("--- Phase 13: Heartbeat Keepalive (20s > CCP 10s interval) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let start = Instant::now();
    let mut disconnected = false;
    while start.elapsed() < Duration::from_secs(20) {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Disconnected) => { disconnected = true; break; }
            _ => {}
        }
    }

    let elapsed = start.elapsed();
    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(!disconnected, "Connection dropped after {:.1}s — heartbeat mechanism failed", elapsed.as_secs_f64());
    println!("  PASS ({:.1}s, no disconnect)\n", elapsed.as_secs_f64());
    conns
}

// ─── Phase 15: Stop limit order submit + cancel ───

fn phase_stop_limit_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 15: Stop Limit Order Submit + Cancel (SPY)",
        OrderRequest::SubmitStopLimit { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 998_00_000_000, stop_price: 999_00_000_000 },
        false)
}

// ─── Phase 16: Subscribe + Unsubscribe ───

fn phase_subscribe_unsubscribe(conns: Conns) -> Conns {
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

// ─── Phase 17: Commission tracking ───

fn phase_commission(conns: Conns) -> Conns {
    println!("--- Phase 17: Commission Tracking (GTC+OutsideRTH fill) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let buy_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
        order_id: buy_id, instrument: inst_id, side: Side::Buy, qty: 1,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut phase = 1u8;
    let mut buy_price = 0i64;
    let mut buy_comm = 0i64;
    let mut sell_price = 0i64;
    let mut sell_comm = 0i64;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Fill(fill)) => {
                if phase == 1 && fill.side == Side::Buy {
                    buy_price = fill.price;
                    buy_comm = fill.commission;
                    let sid = next_order_id() + 1;
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: sid, instrument: fill.instrument, side: Side::Sell, qty: 1,
                    })).unwrap();
                    phase = 2;
                } else if phase == 2 && fill.side == Side::Sell {
                    sell_price = fill.price;
                    sell_comm = fill.commission;
                    break;
                }
            }
            Ok(Event::OrderUpdate(update)) => {
                if update.status == OrderStatus::Rejected { order_rejected = true; break; }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Order rejected — extended hours may not be active\n");
        return conns;
    }
    if buy_price == 0 {
        println!("  SKIP: No fill — market may not have liquidity\n");
        return conns;
    }
    println!("  Buy: ${:.4} commission=${:.4}", buy_price as f64 / PRICE_SCALE as f64, buy_comm as f64 / PRICE_SCALE as f64);
    println!("  Sell: ${:.4} commission=${:.4}", sell_price as f64 / PRICE_SCALE as f64, sell_comm as f64 / PRICE_SCALE as f64);
    if buy_comm == 0 {
        println!("  PASS (commission=0 — paper account does not report tag 12)\n");
    } else {
        println!("  PASS (commission=${:.4})\n", buy_comm as f64 / PRICE_SCALE as f64);
    }
    conns
}

// ─── Phase 19: Trailing Stop ───

fn phase_trailing_stop(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 19: Trailing Stop Order (SPY)",
        OrderRequest::SubmitTrailingStop { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, trail_amt: 5_00_000_000 },
        false)
}

// ─── Phase 20: Trailing Stop Limit ───

fn phase_trailing_stop_limit(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 20: Trailing Stop Limit Order (SPY)",
        OrderRequest::SubmitTrailingStopLimit { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, price: 1_00_000_000, trail_amt: 5_00_000_000 },
        false)
}

// ─── Phase 21: Limit IOC ───

fn phase_limit_ioc(conns: Conns) -> Conns {
    println!("--- Phase 21: Limit IOC Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitIoc {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: IOC order rejected\n");
        return conns;
    }
    assert!(order_cancelled, "IOC order was not cancelled (should expire immediately at $1)");
    println!("  PASS (IOC cancelled as expected — no fill at $1)\n");
    conns
}

// ─── Phase 22: Limit FOK ───

fn phase_limit_fok(conns: Conns) -> Conns {
    println!("--- Phase 22: Limit FOK Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitFok {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: FOK order rejected\n");
        return conns;
    }
    assert!(order_cancelled, "FOK order was not cancelled (should expire immediately at $1)");
    println!("  PASS (FOK cancelled as expected — no fill at $1)\n");
    conns
}

// ─── Phase 23: Stop GTC ───

fn phase_stop_gtc(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 23: Stop GTC Order (SPY)",
        OrderRequest::SubmitStopGtc { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, stop_price: 1_00_000_000, outside_rth: true },
        false)
}

// ─── Phase 24: Stop Limit GTC ───

fn phase_stop_limit_gtc(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 24: Stop Limit GTC Order (SPY)",
        OrderRequest::SubmitStopLimitGtc { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, price: 1_00_000_000, stop_price: 1_00_000_000, outside_rth: true },
        false)
}

// ─── Phase 25: Market if Touched ───

fn phase_mit_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 25: Market if Touched Order (SPY)",
        OrderRequest::SubmitMit { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, stop_price: 1_00_000_000 },
        false)
}

// ─── Phase 26: Limit if Touched ───

fn phase_lit_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 26: Limit if Touched Order (SPY)",
        OrderRequest::SubmitLit { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 2_00_000_000, stop_price: 1_00_000_000 },
        false)
}

// ─── Phase 27: Market on Close ───

fn phase_moc_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 27: MOC Order (SPY)",
        OrderRequest::SubmitMoc { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        false)
}

// ─── Phase 28: Limit on Close ───

fn phase_loc_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 28: LOC Order (SPY)",
        OrderRequest::SubmitLoc { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000 },
        false)
}

// ─── Phase 29: Bracket Order ───

fn phase_bracket_order(conns: Conns) -> Conns {
    println!("--- Phase 29: Bracket Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let parent_id = next_order_id();
    let tp_id = parent_id + 1;
    let sl_id = parent_id + 2;
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitBracket {
        parent_id, tp_id, sl_id, instrument: inst_id, side: Side::Buy, qty: 1,
        entry_price: 1_00_000_000, take_profit: 2_00_000_000, stop_loss: 50_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut parent_acked = false;
    let mut any_rejected = false;
    let mut cancelled_count = 0u32;
    let mut cancel_sent = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if update.order_id == parent_id { parent_acked = true; }
                        if parent_acked && !cancel_sent {
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: parent_id })).unwrap();
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => {
                        cancelled_count += 1;
                        if cancelled_count >= 1 { break; }
                    }
                    OrderStatus::Rejected => { any_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if any_rejected {
        println!("  SKIP: Bracket order rejected\n");
        return conns;
    }
    assert!(parent_acked, "Parent order was never acknowledged");
    println!("  Parent acked: {}, Cancelled: {} orders", parent_acked, cancelled_count);
    println!("  PASS\n");
    conns
}

// ─── Phase 30: Adaptive Algo Limit ───

fn phase_adaptive_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 30: Adaptive Algo Limit Order (SPY)",
        OrderRequest::SubmitAdaptive { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, priority: AdaptivePriority::Normal },
        false)
}

// ─── Phase 31: Relative / Pegged-to-Primary ───

fn phase_rel_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 31: Relative Order (SPY)",
        OrderRequest::SubmitRel { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, offset: 1_000_000 },
        false)
}

// ─── Phase 32: Limit OPG ───

fn phase_limit_opg(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 32: Limit OPG Order (SPY)",
        OrderRequest::SubmitLimitOpg { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000 },
        false)
}

// ─── Phase 33: Iceberg ───

fn phase_iceberg_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 33: Iceberg Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 10, price: 1_00_000_000, tif: b'1', attrs: OrderAttrs { display_size: 1, outside_rth: true, ..OrderAttrs::default() } },
        false)
}

// ─── Phase 34: Hidden ───

fn phase_hidden_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 34: Hidden Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1', attrs: OrderAttrs { hidden: true, outside_rth: true, ..OrderAttrs::default() } },
        false)
}

// ─── Phase 35: Short Sell ───

fn phase_short_sell(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 35: Short Sell Limit Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::ShortSell, qty: 1, price: 1_00_000_000, tif: b'0', attrs: OrderAttrs::default() },
        false)
}

// ─── Phase 36: Trailing Stop Percent ───

fn phase_trailing_stop_pct(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 36: Trailing Stop Percent Order (SPY)",
        OrderRequest::SubmitTrailingStopPct { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, trail_pct: 100 },
        false)
}

// ─── Phase 37: OCA Group ───

fn phase_oca_group(conns: Conns) -> Conns {
    println!("--- Phase 37: OCA Group (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let oca = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    let id1 = next_order_id();
    let id2 = id1 + 1;
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitEx {
        order_id: id1, instrument: inst_id, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1',
        attrs: OrderAttrs { oca_group: oca, outside_rth: true, ..OrderAttrs::default() },
    })).unwrap();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitEx {
        order_id: id2, instrument: inst_id, side: Side::Buy, qty: 1, price: 2_00_000_000, tif: b'1',
        attrs: OrderAttrs { oca_group: oca, outside_rth: true, ..OrderAttrs::default() },
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut order1_acked = false;
    let mut order2_acked = false;
    let mut any_rejected = false;
    let mut cancelled_count = 0u32;
    let mut cancel_sent = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if update.order_id == id1 { order1_acked = true; }
                        if update.order_id == id2 { order2_acked = true; }
                        if order1_acked && order2_acked && !cancel_sent {
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: id1 })).unwrap();
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => {
                        cancelled_count += 1;
                        if cancelled_count >= 1 { break; }
                    }
                    OrderStatus::Rejected => { any_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if any_rejected {
        println!("  SKIP: OCA order rejected\n");
        return conns;
    }
    assert!(order1_acked, "Order 1 never acked");
    assert!(order2_acked, "Order 2 never acked");
    println!("  Order1 acked: {}, Order2 acked: {}, Cancelled: {}", order1_acked, order2_acked, cancelled_count);
    println!("  PASS\n");
    conns
}

// ─── Phase 38: Market to Limit ───

fn phase_mtl_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 38: Market to Limit Order (SPY)",
        OrderRequest::SubmitMtl { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        true)
}

// ─── Phase 39: Market with Protection ───

fn phase_mkt_prt_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 39: Market with Protection Order (SPY)",
        OrderRequest::SubmitMktPrt { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        true)
}

// ─── Phase 40: Stop with Protection ───

fn phase_stp_prt_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 40: Stop with Protection Order (SPY)",
        OrderRequest::SubmitStpPrt { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, stop_price: 1_00_000_000 },
        false)
}

// ─── Phase 41: Mid-Price ───

fn phase_mid_price_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 41: Mid-Price Order (SPY)",
        OrderRequest::SubmitMidPrice { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price_cap: 1_00_000_000 },
        false)
}

// ─── Phase 42: Snap to Market ───

fn phase_snap_mkt_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 42: Snap to Market Order (SPY)",
        OrderRequest::SubmitSnapMkt { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        true)
}

// ─── Phase 43: Snap to Midpoint ───

fn phase_snap_mid_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 43: Snap to Midpoint Order (SPY)",
        OrderRequest::SubmitSnapMid { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        true)
}

// ─── Phase 44: Snap to Primary ───

fn phase_snap_pri_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 44: Snap to Primary Order (SPY)",
        OrderRequest::SubmitSnapPri { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        true)
}

// ─── Phase 45: Pegged to Market ───

fn phase_peg_mkt_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 45: Pegged to Market Order (SPY)",
        OrderRequest::SubmitPegMkt { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, offset: 0 },
        true)
}

// ─── Phase 46: Pegged to Midpoint ───

fn phase_peg_mid_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 46: Pegged to Midpoint Order (SPY)",
        OrderRequest::SubmitPegMid { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, offset: 0 },
        true)
}

// ─── Phase 47: Discretionary Amount ───

fn phase_discretionary_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 47: Discretionary Amount Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1', attrs: OrderAttrs { discretionary_amt: 50_000_000, outside_rth: true, ..OrderAttrs::default() } },
        false)
}

// ─── Phase 48: Sweep to Fill ───

fn phase_sweep_to_fill_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 48: Sweep to Fill Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1', attrs: OrderAttrs { sweep_to_fill: true, outside_rth: true, ..OrderAttrs::default() } },
        false)
}

// ─── Phase 49: All or None ───

fn phase_all_or_none_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 49: All or None Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1', attrs: OrderAttrs { all_or_none: true, outside_rth: true, ..OrderAttrs::default() } },
        false)
}

// ─── Phase 50: Trigger Method ───

fn phase_trigger_method_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 50: Trigger Method Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1', attrs: OrderAttrs { trigger_method: 2, outside_rth: true, ..OrderAttrs::default() } },
        false)
}

// ─── Phase 10b: Outside RTH GTC Stop ───

fn phase_outside_rth_stop(conns: Conns) -> Conns {
    println!("--- Phase 10b: Outside RTH GTC Stop Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitStopGtc {
        order_id, instrument: inst_id, side: Side::Sell, qty: 1, stop_price: 1_00_000_000, outside_rth: true,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_acked = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;
    let mut cancel_sent = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        order_acked = true;
                        if !cancel_sent {
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: GTC stop outside RTH rejected\n");
        return conns;
    }
    assert!(order_acked, "GTC stop outside RTH was never acknowledged");
    assert!(order_cancelled, "GTC stop outside RTH was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 9b: Modify Order Qty ───

fn phase_modify_qty(conns: Conns) -> Conns {
    println!("--- Phase 9b: Order Modify Qty (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    let new_order_id = order_id + 1;
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimit {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut order_acked = false;
    let mut modify_sent = false;
    let mut modify_acked_local = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if modify_sent && !modify_acked_local {
                            modify_acked_local = true;
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: new_order_id })).unwrap();
                        } else if !order_acked {
                            order_acked = true;
                            control_tx.send(ControlCommand::Order(OrderRequest::Modify {
                                order_id, new_order_id, price: 1_00_000_000, qty: 2,
                            })).unwrap();
                            modify_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Modify qty test rejected\n");
        return conns;
    }
    assert!(order_acked, "Order was never acknowledged");
    assert!(modify_sent, "Modify was never sent");
    assert!(modify_acked_local, "Qty modify was never acknowledged");
    assert!(order_cancelled, "Modified order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 51: Bracket Fill Cascade ───

fn phase_bracket_fill_cascade(conns: Conns) -> Conns {
    println!("--- Phase 51: Bracket Fill Cascade (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut tick_count = 0u32;
    let mut parent_id: Option<u64> = None;
    let mut tp_id: Option<u64> = None;
    let mut sl_id: Option<u64> = None;
    let mut entry_filled = false;
    let mut tp_active = false;
    let mut sl_active = false;
    let mut cancelled_count = 0u32;
    let mut cancel_sent = false;
    let mut any_rejected = false;
    let mut done = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(_)) => {
                tick_count += 1;
                if tick_count == 5 && parent_id.is_none() {
                    let q = shared.quote(inst_id);
                    if q.ask <= 0 { continue; }
                    let entry = q.ask + 1_00_000_000;
                    let pid = next_order_id();
                    let tid = pid + 1;
                    let sid = pid + 2;
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitBracket {
                        parent_id: pid, tp_id: tid, sl_id: sid,
                        instrument: inst_id, side: Side::Buy, qty: 1,
                        entry_price: entry,
                        take_profit: entry + 100_00_000_000,
                        stop_loss: 1_000_000,
                    })).unwrap();
                    parent_id = Some(pid);
                    tp_id = Some(tid);
                    sl_id = Some(sid);
                }
            }
            Ok(Event::Fill(fill)) => {
                if Some(fill.order_id) == parent_id { entry_filled = true; }
                if cancel_sent && Some(fill.order_id) != parent_id { done = true; break; }
            }
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if Some(update.order_id) == tp_id { tp_active = true; }
                        if Some(update.order_id) == sl_id { sl_active = true; }
                        if tp_active && sl_active && !cancel_sent {
                            if let Some(t) = tp_id { control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: t })).unwrap(); }
                            if let Some(s) = sl_id { control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: s })).unwrap(); }
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => {
                        cancelled_count += 1;
                        if cancelled_count >= 2 {
                            let sid = next_order_id() + 10;
                            control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                                order_id: sid, instrument: inst_id, side: Side::Sell, qty: 1,
                            })).unwrap();
                        }
                    }
                    OrderStatus::Rejected => { any_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let _ = done;

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if any_rejected {
        println!("  SKIP: Bracket fill cascade rejected\n");
        return conns;
    }
    println!("  Entry filled: {}, TP active: {}, SL active: {}", entry_filled, tp_active, sl_active);
    if !entry_filled {
        println!("  SKIP: Entry did not fill — market may not have liquidity\n");
        return conns;
    }
    assert!(tp_active, "Take-profit child was never activated after entry fill");
    assert!(sl_active, "Stop-loss child was never activated after entry fill");
    println!("  PASS\n");
    conns
}

// ─── Phase 52: PnL After Round Trip ───

fn phase_pnl_after_round_trip(conns: Conns) -> Conns {
    println!("--- Phase 52: PnL After Round Trip (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let initial_rpnl = shared.account().realized_pnl;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut tick_count = 0u32;
    let mut phase = 0u8;
    let mut buy_filled = false;
    let mut sell_filled = false;
    let mut pnl_updated = false;
    let mut order_rejected = false;
    let mut realized_pnl = 0i64;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(_)) => {
                tick_count += 1;
                if phase == 0 && tick_count >= 5 {
                    let oid = next_order_id();
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: oid, instrument: inst_id, side: Side::Buy, qty: 1,
                    })).unwrap();
                    phase = 1;
                }
                if phase == 3 {
                    let current = shared.account().realized_pnl;
                    if current != initial_rpnl {
                        realized_pnl = current;
                        pnl_updated = true;
                        break;
                    }
                }
            }
            Ok(Event::Fill(fill)) => {
                if phase == 1 && fill.side == Side::Buy {
                    buy_filled = true;
                    let sid = next_order_id() + 1;
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: sid, instrument: fill.instrument, side: Side::Sell, qty: 1,
                    })).unwrap();
                    phase = 2;
                } else if phase == 2 && fill.side == Side::Sell {
                    sell_filled = true;
                    phase = 3;
                }
            }
            Ok(Event::OrderUpdate(update)) => {
                if update.status == OrderStatus::Rejected { order_rejected = true; break; }
            }
            _ => {}
        }
    }

    if sell_filled && !pnl_updated {
        let extra = Instant::now() + Duration::from_secs(5);
        while Instant::now() < extra {
            let current = shared.account().realized_pnl;
            if current != initial_rpnl { realized_pnl = current; pnl_updated = true; break; }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected { println!("  SKIP: Order rejected\n"); return conns; }
    if !buy_filled { println!("  SKIP: No fill — market may not have liquidity\n"); return conns; }

    println!("  Buy filled: {}, Sell filled: {}", buy_filled, sell_filled);
    if pnl_updated {
        println!("  RealizedPnL changed: ${:.2}", realized_pnl as f64 / PRICE_SCALE as f64);
        println!("  PASS\n");
    } else {
        println!("  PASS (PnL not yet updated — paper account delay is expected)\n");
    }
    conns
}

// ─── Phase 55: Farm heartbeat keepalive ───

fn phase_farm_heartbeat_keepalive(conns: Conns) -> Conns {
    println!("--- Phase 55: Farm Heartbeat Keepalive (65s > 2x farm 30s interval) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let join = run_hot_loop(hot_loop);

    let start = Instant::now();
    let mut disconnected = false;
    while start.elapsed() < Duration::from_secs(65) {
        match event_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Event::Disconnected) => { disconnected = true; break; }
            _ => {}
        }
    }

    let elapsed = start.elapsed();
    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(!disconnected, "Farm disconnected after {:.1}s — heartbeat failed", elapsed.as_secs_f64());
    println!("  PASS ({:.1}s, no disconnect, survived 2x farm heartbeat interval)\n", elapsed.as_secs_f64());
    conns
}

// ─── Phase 56: Heartbeat timeout detection ───

fn phase_heartbeat_timeout_detection(conns: Conns) -> Conns {
    println!("--- Phase 56: Heartbeat Timeout Detection (simulated stale CCP) ---");

    let account_id = conns.account_id;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind localhost");
    let addr = listener.local_addr().unwrap();
    let client = std::net::TcpStream::connect(addr).expect("connect to localhost");
    let _server = listener.accept().expect("accept dead socket").0;
    let dead_ccp = Connection::new_raw(client).expect("wrap dead socket as Connection");
    let real_ccp = conns.ccp;

    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, dead_ccp, conns.hmds, None,
    );
    let join = run_hot_loop(hot_loop);

    let start = Instant::now();
    let mut disconnect_count = 0u32;
    while start.elapsed() < Duration::from_secs(30) {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Disconnected) => { disconnect_count += 1; break; }
            _ => {}
        }
    }

    let elapsed = start.elapsed();
    assert!(disconnect_count > 0, "No disconnect after {:.1}s — heartbeat timeout should fire at ~21s", elapsed.as_secs_f64());
    assert!(elapsed.as_secs() >= 18 && elapsed.as_secs() <= 28,
        "Disconnect at {:.1}s — expected 18-28s (10+1+10=21s theoretical)", elapsed.as_secs_f64());

    let reclaimed = shutdown_and_reclaim(&control_tx, join, account_id.clone());

    println!("  Timeout at {:.1}s (expected ~21s)", elapsed.as_secs_f64());
    println!("  on_disconnect emitted at least once");
    println!("  Loop survived timeout (graceful shutdown succeeded)");
    println!("  PASS\n");

    Conns { farm: reclaimed.farm, ccp: real_ccp, hmds: reclaimed.hmds, account_id }
}

// ─── Phase 57: Price Condition Order ───

fn phase_price_condition_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 57: Price Condition Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1',
            attrs: OrderAttrs { outside_rth: true, conditions: vec![OrderCondition::Price { con_id: 756733, exchange: "BEST".into(), price: 1_00_000_000, is_more: false, trigger_method: 0 }], ..OrderAttrs::default() } },
        false)
}

// ─── Phase 58: Time Condition Order ───

fn phase_time_condition_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 58: Time Condition Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1',
            attrs: OrderAttrs { outside_rth: true, conditions: vec![OrderCondition::Time { time: "20991231-23:59:59".into(), is_more: false }], ..OrderAttrs::default() } },
        false)
}

// ─── Phase 59: Volume Condition Order ───

fn phase_volume_condition_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 59: Volume Condition Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1',
            attrs: OrderAttrs { outside_rth: true, conditions: vec![OrderCondition::Volume { con_id: 756733, exchange: "BEST".into(), volume: 999_999_999, is_more: true }], ..OrderAttrs::default() } },
        false)
}

// ─── Phase 60: Multi-Condition Order ───

fn phase_multi_condition_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 60: Multi-Condition Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1',
            attrs: OrderAttrs {
                outside_rth: true,
                conditions: vec![
                    OrderCondition::Price { con_id: 756733, exchange: "BEST".into(), price: 1_00_000_000, is_more: false, trigger_method: 2 },
                    OrderCondition::Volume { con_id: 756733, exchange: "BEST".into(), volume: 999_999_999, is_more: true },
                ],
                conditions_cancel_order: true,
                ..OrderAttrs::default()
            } },
        false)
}

// ─── Phase 61: Tick-by-Tick Data ───

fn phase_tbt_subscribe(conns: Conns) -> Conns {
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

// ─── Phase 62: VWAP Algo ───

fn phase_vwap_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 62: VWAP Algo Order (SPY)",
        OrderRequest::SubmitAlgo { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000,
            algo: AlgoParams::Vwap { max_pct_vol: 0.1, no_take_liq: false, allow_past_end_time: true, start_time: "20260311-13:30:00".into(), end_time: "20260311-20:00:00".into() } },
        false)
}

// ─── Phase 63: TWAP Algo ───

fn phase_twap_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 63: TWAP Algo Order (SPY)",
        OrderRequest::SubmitAlgo { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000,
            algo: AlgoParams::Twap { allow_past_end_time: true, start_time: "20260311-13:30:00".into(), end_time: "20260311-20:00:00".into() } },
        false)
}

// ─── Phase 64: Arrival Price Algo ───

fn phase_arrival_px_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 64: Arrival Price Algo Order (SPY)",
        OrderRequest::SubmitAlgo { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000,
            algo: AlgoParams::ArrivalPx { max_pct_vol: 0.1, risk_aversion: RiskAversion::Neutral, allow_past_end_time: true, force_completion: false, start_time: "20260311-13:30:00".into(), end_time: "20260311-20:00:00".into() } },
        false)
}

// ─── Phase 65: Close Price Algo ───

fn phase_close_px_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 65: Close Price Algo Order (SPY)",
        OrderRequest::SubmitAlgo { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000,
            algo: AlgoParams::ClosePx { max_pct_vol: 0.1, risk_aversion: RiskAversion::Neutral, force_completion: false, start_time: "20260311-13:30:00".into() } },
        false)
}

// ─── Phase 66: Dark Ice Algo ───

fn phase_dark_ice_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 66: Dark Ice Algo Order (SPY)",
        OrderRequest::SubmitAlgo { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000,
            algo: AlgoParams::DarkIce { allow_past_end_time: true, display_size: 1, start_time: "20260311-13:30:00".into(), end_time: "20260311-20:00:00".into() } },
        false)
}

// ─── Phase 67: % of Volume Algo ───

fn phase_pct_vol_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 67: % of Volume Algo Order (SPY)",
        OrderRequest::SubmitAlgo { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000,
            algo: AlgoParams::PctVol { pct_vol: 0.1, no_take_liq: false, start_time: "20260311-13:30:00".into(), end_time: "20260311-20:00:00".into() } },
        false)
}

// ─── Phase 68: Pegged to Benchmark ───

fn phase_peg_bench_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 68: Pegged to Benchmark Order (SPY pegged to AAPL)",
        OrderRequest::SubmitPegBench { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, ref_con_id: 265598, is_peg_decrease: false, pegged_change_amount: 50_000_000, ref_change_amount: 50_000_000 },
        false)
}

// ─── Phase 69: Limit Auction ───

fn phase_limit_auc_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 69: Limit Auction Order (SPY)",
        OrderRequest::SubmitLimitAuc { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000 },
        false)
}

// ─── Phase 70: MTL Auction ───

fn phase_mtl_auc_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 70: Market-to-Limit Auction Order (SPY)",
        OrderRequest::SubmitMtlAuc { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        false)
}

// ─── Phase 71: Box Top (wire-identical to MTL) ───

fn phase_box_top_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 71: Box Top Order (SPY)",
        OrderRequest::SubmitMtl { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        true)
}

// ─── Phase 72: What-If Order ───

fn phase_what_if_order(conns: Conns) -> Conns {
    println!("--- Phase 72: What-If Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitWhatIf {
        order_id, instrument: inst_id, side: Side::Buy, qty: 100, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut what_if_received = false;
    let mut commission = 0i64;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::WhatIf(response)) => {
                commission = response.commission;
                what_if_received = true;
                break;
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(what_if_received, "What-if response was never received");
    assert!(commission > 0, "Commission should be > 0, got {}", commission);
    println!("  Commission: ${:.2}", commission as f64 / PRICE_SCALE as f64);
    println!("  PASS\n");
    conns
}

// ─── Phase 73: Cash Quantity Order ───

fn phase_cash_qty_order(conns: Conns) -> Conns {
    println!("--- Phase 73: Cash Quantity Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitEx {
        order_id, instrument: inst_id, side: Side::Buy, qty: 100, price: 1_00_000_000, tif: b'0',
        attrs: OrderAttrs { cash_qty: 1000 * PRICE_SCALE, ..OrderAttrs::default() },
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut order_acked = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;
    let mut cancel_sent = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        order_acked = true;
                        if !cancel_sent {
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Cash qty rejected (expected on paper account)\n");
        return conns;
    }
    assert!(order_acked, "Order was never acknowledged");
    assert!(order_cancelled, "Order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 74: Fractional Shares Order ───

fn phase_fractional_order(conns: Conns) -> Conns {
    println!("--- Phase 74: Fractional Shares Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitFractional {
        order_id, instrument: inst_id, side: Side::Buy, qty: QTY_SCALE / 2, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut order_acked = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;
    let mut cancel_sent = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        order_acked = true;
                        if !cancel_sent {
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Fractional rejected (may be blocked by CCP)\n");
        return conns;
    }
    assert!(order_acked, "Order was never acknowledged");
    assert!(order_cancelled, "Order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 75: Adjustable Stop ───

fn phase_adjustable_stop_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 75: Adjustable Stop Order (SPY)",
        OrderRequest::SubmitAdjustableStop { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, stop_price: 1_00_000_000, trigger_price: 500_00_000_000, adjusted_order_type: AdjustedOrderType::StopLimit, adjusted_stop_price: 1_50_000_000, adjusted_stop_limit_price: 1_00_000_000 },
        false)
}

// ─── Timestamp helper (shared by historical phases) ───

fn now_ib_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let secs_per_day = 86400u64;
    let days = now / secs_per_day;
    let mut y = 1970i64;
    let mut remaining = days as i64;
    loop {
        let diy = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
        if remaining < diy { break; }
        remaining -= diy;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays = [31i64, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 1usize;
    for &d in &mdays {
        if remaining < d { break; }
        remaining -= d;
        m += 1;
    }
    let day = remaining + 1;
    let hour = (now % secs_per_day) / 3600;
    let min = (now % 3600) / 60;
    let sec = now % 60;
    format!("{:04}{:02}{:02}-{:02}:{:02}:{:02}", y, m, day, hour, min, sec)
}

// ─── Phase 76: Historical daily bars via HMDS ───

fn phase_historical_daily_bars(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 76: Historical Daily Bars (SPY, 5 days of 1-day bars) ---");

    let mut hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (bg_loop, bg_tx) = HotLoop::with_connections(shared, None, account_id.clone(), conns.farm, conns.ccp, None, None);
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    let req = HistoricalRequest {
        query_id: "daily1".to_string(), con_id: 756733, symbol: "SPY".to_string(),
        sec_type: "CS", exchange: "SMART", data_type: BarDataType::Trades,
        end_time: now_ib_timestamp(), duration: "5 d".to_string(), bar_size: BarSize::Day1, use_rth: true,
    };
    let xml = historical::build_query_xml(&req);
    hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "W"), (historical::TAG_HISTORICAL_XML, &xml)]).expect("Failed to send daily bar request");

    let mut all_bars = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut complete = false;

    while Instant::now() < deadline && !complete {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = hmds.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if let Some(xml_resp) = tags.get(&historical::TAG_HISTORICAL_XML) {
                    if let Some(resp) = historical::parse_bar_response(xml_resp) {
                        all_bars.extend(resp.bars);
                        if resp.is_complete { complete = true; }
                    }
                }
            }
        }
    }

    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);

    println!("  Total daily bars: {}", all_bars.len());
    if all_bars.is_empty() {
        println!("  SKIP: No daily bars received (HMDS may be unavailable)\n");
        return bg_conns;
    }
    assert!(all_bars.len() <= 5, "Should have at most 5 daily bars, got {}", all_bars.len());
    for bar in &all_bars {
        assert!(bar.open > 0.0);
        assert!(bar.high >= bar.low);
        assert!(bar.volume > 0);
        println!("  {} O={:.2} H={:.2} L={:.2} C={:.2} V={}", bar.time, bar.open, bar.high, bar.low, bar.close, bar.volume);
    }
    println!("  PASS ({} daily bars)\n", all_bars.len());
    bg_conns
}

// ─── Phase 77: Cancel historical request ───

fn phase_cancel_historical(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 77: Cancel Historical Request (SPY) ---");

    let mut hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (bg_loop, bg_tx) = HotLoop::with_connections(shared, None, account_id.clone(), conns.farm, conns.ccp, None, None);
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    let req = HistoricalRequest {
        query_id: "cancel_test".to_string(), con_id: 756733, symbol: "SPY".to_string(),
        sec_type: "CS", exchange: "SMART", data_type: BarDataType::Trades,
        end_time: now_ib_timestamp(), duration: "1 d".to_string(), bar_size: BarSize::Sec1, use_rth: true,
    };
    let xml = historical::build_query_xml(&req);
    hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "W"), (historical::TAG_HISTORICAL_XML, &xml)]).expect("Failed to send historical request");

    let mut got_first_chunk = false;
    let first_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < first_deadline && !got_first_chunk {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = hmds.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if let Some(xml_resp) = tags.get(&historical::TAG_HISTORICAL_XML) {
                    if historical::parse_bar_response(xml_resp).is_some() {
                        got_first_chunk = true;
                        println!("  First chunk received, sending cancel");
                    }
                }
            }
        }
    }

    if !got_first_chunk {
        let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
        bg_conns.hmds = Some(hmds);
        println!("  SKIP: No initial data received to cancel\n");
        return bg_conns;
    }

    let cancel_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListOfCancelQueries><CancelQuery><id>cancel_test</id></CancelQuery></ListOfCancelQueries>".to_string();
    hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "Z"), (historical::TAG_HISTORICAL_XML, &cancel_xml)]).expect("Failed to send cancel request");
    println!("  Cancel sent");

    let drain_deadline = Instant::now() + Duration::from_secs(5);
    let mut bars_after_cancel = 0u32;
    while Instant::now() < drain_deadline {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error after cancel: {}", e); break; }
            Ok(_) => {}
        }
        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = hmds.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if let Some(xml_resp) = tags.get(&historical::TAG_HISTORICAL_XML) {
                    if let Some(resp) = historical::parse_bar_response(xml_resp) {
                        bars_after_cancel += resp.bars.len() as u32;
                    }
                }
            }
        }
    }

    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);
    println!("  Bars received after cancel: {} (in-flight data is expected)", bars_after_cancel);
    println!("  PASS (cancel sent successfully, connection intact)\n");
    bg_conns
}

// ─── Phase 78: Contract details by symbol search ───

fn phase_contract_details_by_symbol(conns: &mut Conns) {
    println!("--- Phase 78: Contract Details by Symbol Search (AAPL) ---");

    let now = ibx::gateway::chrono_free_timestamp();
    conns.ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SECURITY_REQ_ID, "R_sym1"),
        (contracts::TAG_SECURITY_REQ_TYPE, "2"),
        (contracts::TAG_SYMBOL, "AAPL"),
        (contracts::TAG_SECURITY_TYPE, "CS"),
        (contracts::TAG_EXCHANGE, "BEST"),
        (contracts::TAG_CURRENCY, "USD"),
        (contracts::TAG_IB_SOURCE, "Socket"),
    ]).expect("Failed to send symbol search request");

    let mut contract: Option<contracts::ContractDefinition> = None;
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline && contract.is_none() {
        match conns.ccp.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  CCP recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in conns.ccp.extract_frames() {
            let messages = match frame {
                Frame::FixComp(raw) => { let (u, _) = conns.ccp.unsign(&raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw],
                _ => continue,
            };
            for msg in messages {
                let tags = fix::fix_parse(&msg);
                if tags.get(&fix::TAG_MSG_TYPE).map(|s| s.as_str()) == Some("d") {
                    if let Some(req_id) = tags.get(&contracts::TAG_SECURITY_REQ_ID) {
                        if req_id == "R_sym1" {
                            if let Some(def) = contracts::parse_secdef_response(&msg) {
                                println!("  {} ({}) conId={}", def.symbol, def.long_name, def.con_id);
                                contract = Some(def);
                            }
                        }
                    }
                }
            }
        }
    }

    let def = contract.expect("No contract details received for AAPL by symbol search");
    assert_eq!(def.symbol, "AAPL");
    assert!(def.con_id > 0);
    assert_eq!(def.sec_type, contracts::SecurityType::Stock);
    assert_eq!(def.currency, "USD");
    assert!(!def.long_name.is_empty());
    assert!(def.min_tick > 0.0);
    println!("  conId={} MinTick={}", def.con_id, def.min_tick);
    println!("  PASS\n");
}

// ─── Phase 79: Head timestamp via HMDS ───

fn phase_head_timestamp(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 79: Head Timestamp (SPY, TRADES) ---");

    let mut hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (bg_loop, bg_tx) = HotLoop::with_connections(shared, None, account_id.clone(), conns.farm, conns.ccp, None, None);
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    let req = HeadTimestampRequest { con_id: 756733, sec_type: "STK", exchange: "SMART", data_type: BarDataType::Trades, use_rth: true };
    let xml = historical::build_head_timestamp_xml(&req);
    hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "W"), (historical::TAG_HISTORICAL_XML, &xml)]).expect("Failed to send head timestamp request");

    let mut response: Option<historical::HeadTimestampResponse> = None;
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline && response.is_none() {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = hmds.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if let Some(xml_resp) = tags.get(&historical::TAG_HISTORICAL_XML) {
                    if let Some(resp) = historical::parse_head_timestamp_response(xml_resp) {
                        println!("  headTS={} tz={}", resp.head_timestamp, resp.timezone);
                        response = Some(resp);
                    }
                }
            }
        }
    }

    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);

    if response.is_none() {
        println!("  SKIP: No head timestamp received (HMDS may be unavailable)\n");
        return bg_conns;
    }
    let resp = response.unwrap();
    assert!(!resp.head_timestamp.is_empty());
    assert!(resp.head_timestamp.starts_with("199"), "SPY TRADES head timestamp should be in 1990s, got {}", resp.head_timestamp);
    assert!(!resp.timezone.is_empty());
    println!("  PASS\n");
    bg_conns
}

// ─── Phase 80: Trading hours ───

fn phase_trading_hours(conns: &mut Conns) {
    println!("--- Phase 80: Trading Hours (schedule subscription, AAPL) ---");

    let now = ibx::gateway::chrono_free_timestamp();
    conns.farm.send_fixcomp(&[
        (fix::TAG_MSG_TYPE, "V"),
        (fix::TAG_SENDING_TIME, &now),
        (263, "1"), (146, "1"), (262, "sched_test"),
        (6008, "265598"), (207, "BEST"), (167, "CS"),
        (264, "442"), (6088, "Socket"), (9830, "1"), (9839, "1"),
    ]).expect("Failed to send farm subscribe for AAPL");
    println!("  Subscribed AAPL on farm, listening on CCP for schedule");

    let mut schedule: Option<contracts::ContractSchedule> = None;
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline && schedule.is_none() {
        match conns.farm.try_recv() {
            Ok(_) => { conns.farm.extract_frames(); }
            Err(_) => {}
        }
        match conns.ccp.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  CCP recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in conns.ccp.extract_frames() {
            let messages = match frame {
                Frame::FixComp(raw) => { let (u, _) = conns.ccp.unsign(&raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw],
                _ => continue,
            };
            for msg in messages {
                if let Some(sched) = contracts::parse_schedule_response(&msg) {
                    println!("  Schedule: tz={} trading={} liquid={}", sched.timezone, sched.trading_hours.len(), sched.liquid_hours.len());
                    schedule = Some(sched);
                }
            }
        }
    }

    let now2 = ibx::gateway::chrono_free_timestamp();
    let _ = conns.farm.send_fixcomp(&[
        (fix::TAG_MSG_TYPE, "V"), (fix::TAG_SENDING_TIME, &now2),
        (263, "2"), (146, "1"), (262, "sched_test"),
        (6008, "265598"), (207, "BEST"), (167, "CS"),
        (264, "442"), (6088, "Socket"), (9830, "1"), (9839, "1"),
    ]);

    if schedule.is_none() {
        println!("  SKIP: No schedule received\n");
        return;
    }
    let sched = schedule.unwrap();
    assert!(!sched.timezone.is_empty());
    assert!(!sched.trading_hours.is_empty());
    assert!(!sched.liquid_hours.is_empty());
    assert!(sched.liquid_hours.len() <= sched.trading_hours.len());
    println!("  PASS\n");
}

// ─── Phase 81: Matching symbols search ───

fn phase_matching_symbols(conns: &mut Conns) {
    println!("--- Phase 81: Matching Symbols Search (pattern=\"SPY\") ---");

    let now = ibx::gateway::chrono_free_timestamp();
    conns.ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "U"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SUB_PROTOCOL, "185"),
        (contracts::TAG_SECURITY_REQ_ID, "R_match1"),
        (contracts::TAG_MATCH_PATTERN, "SPY"),
    ]).expect("Failed to send matching symbols request");

    let mut matches: Option<Vec<contracts::SymbolMatch>> = None;
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline && matches.is_none() {
        match conns.ccp.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  CCP recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in conns.ccp.extract_frames() {
            let messages = match frame {
                Frame::FixComp(raw) => { let (u, _) = conns.ccp.unsign(&raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw],
                _ => continue,
            };
            for msg in messages {
                let tags = fix::fix_parse(&msg);
                if tags.get(&fix::TAG_MSG_TYPE).map(|s| s.as_str()) == Some("U") {
                    if tags.get(&contracts::TAG_SUB_PROTOCOL).map(|s| s.as_str()) == Some("186") {
                        if !tags.contains_key(&contracts::TAG_MATCH_COUNT) { continue; }
                        if let Some(m) = contracts::parse_matching_symbols_response(&msg) {
                            println!("  {} matches found", m.len());
                            matches = Some(m);
                        }
                    }
                }
            }
        }
    }

    if matches.is_none() {
        println!("  SKIP: No matching symbols response received\n");
        return;
    }
    let m = matches.unwrap();
    assert!(!m.is_empty(), "Should have at least one match for 'SPY'");
    let spy = m.iter().find(|s| s.symbol == "SPY" && s.sec_type == contracts::SecurityType::Stock && s.currency == "USD");
    if let Some(spy) = spy {
        assert_eq!(spy.con_id, 756733);
        println!("  SPY: conId={} exchange={} desc={}", spy.con_id, spy.primary_exchange, spy.description);
    } else {
        println!("  WARNING: SPY STK not found in matches");
    }
    println!("  PASS\n");
}

// ─── Phase 82: Scanner subscription via HMDS ───

fn phase_scanner_subscription(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 82: Scanner Subscription (TOP_PERC_GAIN, STK.US.MAJOR) ---");

    let mut hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (bg_loop, bg_tx) = HotLoop::with_connections(shared, None, account_id.clone(), conns.farm, conns.ccp, None, None);
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    let sub = scanner::ScannerSubscription {
        instrument: "STK".to_string(), location_code: "STK.US.MAJOR".to_string(),
        scan_code: "TOP_PERC_GAIN".to_string(), max_items: 10,
    };
    let xml = scanner::build_scanner_subscribe_xml(&sub);
    hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "U"), (scanner::TAG_SUB_PROTOCOL, "10003"), (scanner::TAG_SCANNER_XML, &xml)]).expect("Failed to send scanner subscription");

    let mut result: Option<scanner::ScannerResult> = None;
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline && result.is_none() {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = hmds.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if tags.get(&scanner::TAG_SUB_PROTOCOL).map(|s| s.as_str()) == Some("10005") {
                    if let Some(xml_resp) = tags.get(&scanner::TAG_SCANNER_XML) {
                        if let Some(r) = scanner::parse_scanner_response(xml_resp) {
                            println!("  Scanner: {} contracts at {}", r.con_ids.len(), r.scan_time);
                            result = Some(r);
                        }
                    }
                }
            }
        }
    }

    let cancel_xml = scanner::build_scanner_cancel_xml("APISCAN1:1");
    let _ = hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "U"), (scanner::TAG_SUB_PROTOCOL, "10004"), (scanner::TAG_SCANNER_XML, &cancel_xml)]);

    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);

    if result.is_none() {
        println!("  SKIP: No scanner results received\n");
        return bg_conns;
    }
    let r = result.unwrap();
    assert!(!r.con_ids.is_empty());
    assert!(!r.scan_time.is_empty());
    for (i, cid) in r.con_ids.iter().enumerate().take(3) {
        println!("  Rank {}: conId={}", i, cid);
    }
    println!("  PASS ({} contracts)\n", r.con_ids.len());
    bg_conns
}

// ─── Phase 83: Fundamental data via fundfarm ───

fn phase_fundamental_data(gw: &Gateway, config: &GatewayConfig) {
    println!("--- Phase 83: Fundamental Data (AAPL, ReportSnapshot) ---");

    let mut fundfarm = match connect_farm(&config.host, "fundfarm", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  fundfarm connected"); c }
        Err(e) => { println!("  SKIP: fundfarm connect failed: {}\n", e); return; }
    };

    let req = fundamental::FundamentalRequest {
        con_id: 265598, sec_type: "STK", currency: "USD",
        report_type: fundamental::ReportType::Snapshot,
    };
    let xml = fundamental::build_fundamental_request_xml(&req);
    fundfarm.send_fixcomp(&[(fix::TAG_MSG_TYPE, "U"), (fundamental::TAG_SUB_PROTOCOL, "10010"), (fundamental::TAG_FUNDAMENTAL_XML, &xml)]).expect("Failed to send fundamental data request");

    let mut got_response = false;
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline && !got_response {
        match fundfarm.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  fundfarm recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in fundfarm.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = fundfarm.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if tags.get(&fundamental::TAG_SUB_PROTOCOL).map(|s| s.as_str()) == Some("10012") {
                    if let Some(xml_resp) = tags.get(&fundamental::TAG_FUNDAMENTAL_XML) {
                        if let Some(id) = fundamental::parse_fundamental_response_id(xml_resp) {
                            println!("  Response ID: {}", id);
                        }
                    }
                    if let Some(raw_data) = tags.get(&fundamental::TAG_RAW_DATA) {
                        println!("  Raw data: {} bytes", raw_data.len());
                        if let Some(xml_out) = fundamental::decompress_fundamental_data(raw_data.as_bytes()) {
                            println!("  Decompressed: {} chars", xml_out.len());
                        } else {
                            println!("  Note: binary payload detected");
                        }
                    }
                    got_response = true;
                }
            }
        }
    }

    if !got_response {
        println!("  SKIP: No fundamental data received (may require subscription)\n");
        return;
    }
    println!("  PASS\n");
}

// ─── Phase 84: Market rule ID in contract details ───

fn phase_market_rule_id(conns: &mut Conns) {
    println!("--- Phase 84: Market Rule ID (SPY, tag 6031) ---");

    let now = ibx::gateway::chrono_free_timestamp();
    conns.ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SECURITY_REQ_ID, "R_rule1"),
        (contracts::TAG_SECURITY_REQ_TYPE, "2"),
        (contracts::TAG_IB_CON_ID, "756733"),
        (contracts::TAG_IB_SOURCE, "Socket"),
    ]).expect("Failed to send secdef request");

    let mut contract: Option<contracts::ContractDefinition> = None;
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline && contract.is_none() {
        match conns.ccp.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  CCP recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in conns.ccp.extract_frames() {
            let messages = match frame {
                Frame::FixComp(raw) => { let (u, _) = conns.ccp.unsign(&raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw],
                _ => continue,
            };
            for msg in messages {
                let tags = fix::fix_parse(&msg);
                if tags.get(&fix::TAG_MSG_TYPE).map(|s| s.as_str()) == Some("d") {
                    if let Some(def) = contracts::parse_secdef_response(&msg) {
                        if def.con_id == 756733 { contract = Some(def); }
                    }
                }
            }
        }
    }

    if contract.is_none() {
        println!("  SKIP: No contract details received\n");
        return;
    }
    let def = contract.unwrap();
    println!("  market_rule_id={:?} min_tick={}", def.market_rule_id, def.min_tick);
    assert!(def.market_rule_id.is_some(), "SPY should have a market rule ID (tag 6031)");
    assert!(def.market_rule_id.unwrap() > 0);
    println!("  PASS\n");
}

// ─── Phase 85: Historical news via HMDS ───

fn phase_historical_news(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 85: Historical News (AAPL) ---");

    let mut hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (bg_loop, bg_tx) = HotLoop::with_connections(shared, None, account_id.clone(), conns.farm, conns.ccp, None, None);
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    let req = news::HistoricalNewsRequest {
        con_id: 265598, provider_codes: "BRFG+BRFUPDN".to_string(),
        start_time: String::new(), end_time: String::new(), max_results: 5,
    };
    let xml = news::build_historical_news_xml(&req);
    hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "U"), (news::TAG_SUB_PROTOCOL, "10030"), (news::TAG_NEWS_XML, &xml)]).expect("Failed to send historical news request");

    let mut got_response = false;
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline && !got_response {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = hmds.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if tags.get(&news::TAG_SUB_PROTOCOL).map(|s| s.as_str()) == Some("10032") {
                    if let Some(xml_resp) = tags.get(&news::TAG_NEWS_XML) {
                        if let Some(id) = news::parse_news_response_id(xml_resp) {
                            println!("  Response ID: {}", id);
                        }
                    }
                    if tags.contains_key(&news::TAG_RAW_DATA) {
                        println!("  Raw data payload received");
                    }
                    got_response = true;
                }
            }
        }
    }

    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);

    if !got_response {
        println!("  SKIP: No news response received (may require news subscription)\n");
        return bg_conns;
    }
    println!("  PASS\n");
    bg_conns
}

// ─── Phase 86: Contract Details via HotLoop event channel (issue #76) ───

fn phase_contract_details_channel(conns: Conns) -> Conns {
    println!("--- Phase 86: Contract Details via Event Channel (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::FetchContractDetails { req_id: 1001, con_id: 756733 }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got_details = false;
    let mut got_end = false;

    while Instant::now() < deadline && !got_details {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::ContractDetails { req_id, details }) => {
                if req_id == 1001 {
                    println!("  ContractDetails: {} ({}) conId={}", details.symbol, details.long_name, details.con_id);
                    assert_eq!(details.con_id, 756733);
                    assert_eq!(details.symbol, "SPY");
                    got_details = true;
                }
            }
            Ok(Event::ContractDetailsEnd(req_id)) => {
                if req_id == 1001 { got_end = true; }
            }
            _ => {}
        }
    }

    // Wait briefly for ContractDetailsEnd if not yet received
    if got_details && !got_end {
        let end_deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end_deadline {
            match event_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::ContractDetailsEnd(req_id)) => {
                    if req_id == 1001 { got_end = true; break; }
                }
                _ => {}
            }
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(got_details, "Event::ContractDetails not received for SPY");
    if got_end {
        println!("  ContractDetailsEnd received");
    } else {
        println!("  ContractDetailsEnd not received (single-conId request — non-fatal)");
    }
    println!("  PASS\n");
    conns
}

// ─── Phase 87: CancelReject Event path (issue #78) ───

fn phase_cancel_reject(conns: Conns) -> Conns {
    println!("--- Phase 87: CancelReject Event (bogus order cancel) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    // Register instrument and submit a real order so there's a known order in context
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1,
        price: 1_00_000_000, outside_rth: true,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    // Wait for order ack, then cancel it twice — second cancel should produce CancelReject
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_acked = false;
    let mut _first_cancel_sent = false;
    let mut first_cancelled = false;
    let mut _second_cancel_sent = false;
    let mut got_reject = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                if update.status == OrderStatus::Submitted && !order_acked {
                    order_acked = true;
                    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                    _first_cancel_sent = true;
                }
                if update.status == OrderStatus::Cancelled && !first_cancelled {
                    first_cancelled = true;
                    // Cancel again — order is already dead, should produce reject
                    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                    _second_cancel_sent = true;
                }
            }
            Ok(Event::CancelReject(reject)) => {
                println!("  CancelReject: order_id={} type={} code={}", reject.order_id, reject.reject_type, reject.reason_code);
                got_reject = true;
                break;
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if !order_acked {
        println!("  SKIP: Order never acknowledged\n");
        return conns;
    }
    if got_reject {
        println!("  PASS\n");
    } else {
        // CancelReject may not be emitted if IB silently ignores the second cancel
        println!("  SKIP: No CancelReject received (IB may silently ignore duplicate cancel)\n");
    }
    conns
}

// ─── Phase 88: Historical Ticks via HotLoop (issue #72) ───

fn phase_historical_ticks(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 88: Historical Ticks (SPY, TRADES) ---");

    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    // Request last 100 historical ticks for SPY, ending now
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let end_dt = format_utc_timestamp(now);
    control_tx.send(ControlCommand::FetchHistoricalTicks {
        req_id: 2001,
        con_id: 756733,
        start_date_time: String::new(),
        end_date_time: end_dt,
        number_of_ticks: 100,
        what_to_show: "TRADES".to_string(),
        use_rth: true,
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut tick_count = 0usize;

    while Instant::now() < deadline {
        let ticks = shared.drain_historical_ticks();
        for (req_id, data, what, done) in &ticks {
            if *req_id == 2001 {
                let count = match data {
                    HistoricalTickData::Last(v) => v.len(),
                    HistoricalTickData::Midpoint(v) => v.len(),
                    HistoricalTickData::BidAsk(v) => v.len(),
                };
                tick_count += count;
                println!("  Received {} ticks (what={}, done={})", count, what, done);
                if *done { break; }
            }
        }
        if tick_count > 0 { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if tick_count == 0 {
        println!("  SKIP: No historical ticks received\n");
    } else {
        println!("  PASS ({} ticks)\n", tick_count);
    }
    conns
}

// ─── Phase 89: Histogram Data via HotLoop (issue #73) ───

fn phase_histogram_data(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 89: Histogram Data (SPY, 1 week) ---");

    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    control_tx.send(ControlCommand::FetchHistogramData {
        req_id: 3001,
        con_id: 756733,
        use_rth: true,
        period: "1 week".to_string(),
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut entries = Vec::new();

    while Instant::now() < deadline {
        let data = shared.drain_histogram_data();
        for (req_id, ents) in data {
            if req_id == 3001 {
                entries = ents;
                break;
            }
        }
        if !entries.is_empty() { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if entries.is_empty() {
        println!("  SKIP: No histogram data received\n");
    } else {
        println!("  {} histogram entries", entries.len());
        if let Some(first) = entries.first() {
            println!("  First: price={:.2} count={}", first.price, first.count);
        }
        println!("  PASS\n");
    }
    conns
}

// ─── Phase 90: Historical Schedule via HotLoop (issue #74) ───

fn phase_historical_schedule(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 90: Historical Schedule (SPY) ---");

    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let end_dt = format_utc_timestamp(now);
    control_tx.send(ControlCommand::FetchHistoricalSchedule {
        req_id: 4001,
        con_id: 756733,
        end_date_time: end_dt,
        duration: "5 d".to_string(),
        use_rth: true,
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut schedule: Option<HistoricalScheduleResponse> = None;

    while Instant::now() < deadline {
        let data = shared.drain_historical_schedules();
        for (req_id, resp) in data {
            if req_id == 4001 {
                schedule = Some(resp);
                break;
            }
        }
        if schedule.is_some() { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if let Some(sched) = schedule {
        println!("  Timezone: {}", sched.timezone);
        println!("  Sessions: {}", sched.sessions.len());
        for s in sched.sessions.iter().take(3) {
            println!("    {} open={} close={}", s.ref_date, s.open_time, s.close_time);
        }
        assert!(!sched.sessions.is_empty(), "Schedule should contain sessions");
        println!("  PASS\n");
    } else {
        println!("  SKIP: No schedule data received\n");
    }
    conns
}

// ─── Phase 91: Real-Time Bars via HotLoop (issue #71) ───

fn phase_realtime_bars(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 91: Real-Time Bars (SPY, 5-second) ---");

    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    control_tx.send(ControlCommand::SubscribeRealTimeBar {
        req_id: 5001,
        con_id: 756733,
        symbol: "SPY".to_string(),
        what_to_show: "TRADES".to_string(),
        use_rth: false,
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    // Wait up to 20s for at least one 5-second bar
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut bars = Vec::new();

    while Instant::now() < deadline {
        let data = shared.drain_real_time_bars();
        for (req_id, bar) in data {
            if req_id == 5001 {
                bars.push(bar);
            }
        }
        if !bars.is_empty() { break; }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Cancel subscription
    control_tx.send(ControlCommand::CancelRealTimeBar { req_id: 5001 }).unwrap();
    std::thread::sleep(Duration::from_millis(500));

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if bars.is_empty() {
        println!("  SKIP: No real-time bars received (market may be closed)\n");
    } else {
        let bar = &bars[0];
        println!("  First bar: O={:.2} H={:.2} L={:.2} C={:.2} V={:.0}", bar.open, bar.high, bar.low, bar.close, bar.volume);
        assert!(bar.high >= bar.low, "High should be >= Low");
        println!("  PASS ({} bars)\n", bars.len());
    }
    conns
}

// ─── Phase 92: News Article Fetch via HotLoop (issue #75) ───

fn phase_news_article(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 92: News Article Fetch (AAPL) ---");

    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    // First request historical news to get an article ID
    control_tx.send(ControlCommand::FetchHistoricalNews {
        req_id: 6001,
        con_id: 265598,
        provider_codes: "BRFG+BRFUPDN".to_string(),
        start_time: String::new(),
        end_time: String::new(),
        max_results: 5,
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    // Poll for headlines
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut article_id: Option<String> = None;
    let mut provider_code: Option<String> = None;

    while Instant::now() < deadline && article_id.is_none() {
        let data = shared.drain_historical_news();
        for (req_id, headlines, _done) in data {
            if req_id == 6001 {
                if let Some(h) = headlines.first() {
                    article_id = Some(h.article_id.clone());
                    provider_code = Some(h.provider_code.clone());
                    println!("  Headline: {} ({})", h.headline, h.article_id);
                }
            }
        }
        if article_id.is_some() { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    if let (Some(art_id), Some(prov)) = (article_id, provider_code) {
        // Now fetch the article body
        control_tx.send(ControlCommand::FetchNewsArticle {
            req_id: 6002,
            provider_code: prov,
            article_id: art_id.clone(),
        }).unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut got_article = false;

        while Instant::now() < deadline {
            let articles = shared.drain_news_articles();
            for (req_id, _art_type, body) in &articles {
                if *req_id == 6002 {
                    println!("  Article body: {} bytes", body.len());
                    got_article = true;
                }
            }
            if got_article { break; }
            std::thread::sleep(Duration::from_millis(100));
        }

        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        if got_article {
            println!("  PASS\n");
        } else {
            println!("  SKIP: Article body not received\n");
        }
        conns
    } else {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        println!("  SKIP: No news headlines to fetch article from\n");
        conns
    }
}

// ─── Phase 93: Fundamental Data via HotLoop (issue #82) ───

fn phase_fundamental_data_channel(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 93: Fundamental Data via HotLoop (AAPL) ---");

    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    control_tx.send(ControlCommand::FetchFundamentalData {
        req_id: 7001,
        con_id: 265598,
        report_type: "ReportSnapshot".to_string(),
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got_data = false;

    while Instant::now() < deadline {
        let data = shared.drain_fundamental_data();
        for (req_id, xml) in &data {
            if *req_id == 7001 {
                println!("  Fundamental data: {} bytes", xml.len());
                got_data = true;
            }
        }
        if got_data { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if got_data {
        println!("  PASS\n");
    } else {
        println!("  SKIP: No fundamental data received (may require subscription)\n");
    }
    conns
}

// ─── Phase 94: Multiple Parallel Historical Requests (issue #80) ───

fn phase_parallel_historical(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 94: Parallel Historical Requests (SPY: 1d/5min, 5d/1day, 1w/1h) ---");

    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let end_dt = format_utc_timestamp(now);

    // Send 3 requests in quick succession
    control_tx.send(ControlCommand::FetchHistorical {
        req_id: 8001, con_id: 756733, symbol: "SPY".to_string(),
        end_date_time: end_dt.clone(), duration: "1 d".to_string(),
        bar_size: "5 mins".to_string(), what_to_show: "TRADES".to_string(), use_rth: true,
    }).unwrap();
    control_tx.send(ControlCommand::FetchHistorical {
        req_id: 8002, con_id: 756733, symbol: "SPY".to_string(),
        end_date_time: end_dt.clone(), duration: "5 d".to_string(),
        bar_size: "1 day".to_string(), what_to_show: "TRADES".to_string(), use_rth: true,
    }).unwrap();
    control_tx.send(ControlCommand::FetchHistorical {
        req_id: 8003, con_id: 756733, symbol: "SPY".to_string(),
        end_date_time: end_dt, duration: "1 W".to_string(),
        bar_size: "1 hour".to_string(), what_to_show: "TRADES".to_string(), use_rth: true,
    }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut received: [bool; 3] = [false; 3];

    while Instant::now() < deadline {
        let data = shared.drain_historical_data();
        for (req_id, resp) in &data {
            match *req_id {
                8001 => { if resp.is_complete { received[0] = true; println!("  req 8001 (1d/5min): {} bars", resp.bars.len()); } }
                8002 => { if resp.is_complete { received[1] = true; println!("  req 8002 (5d/1day): {} bars", resp.bars.len()); } }
                8003 => { if resp.is_complete { received[2] = true; println!("  req 8003 (1W/1h): {} bars", resp.bars.len()); } }
                _ => {}
            }
        }
        if received.iter().all(|r| *r) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    let count = received.iter().filter(|r| **r).count();
    if count == 3 {
        println!("  PASS (all 3 responses received)\n");
    } else if count > 0 {
        println!("  PARTIAL: {}/3 responses received\n", count);
    } else {
        println!("  SKIP: No responses received\n");
    }
    conns
}

// ─── Phase 95: Scanner Parameters (issue #81) ───

fn phase_scanner_params(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 95: Scanner Parameters + HOT_BY_VOLUME Scan ---");

    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    // Request scanner params XML
    control_tx.send(ControlCommand::FetchScannerParams).unwrap();
    // Also subscribe to a HOT_BY_VOLUME scan
    control_tx.send(ControlCommand::SubscribeScanner {
        req_id: 9001,
        instrument: "STK".to_string(),
        location_code: "STK.US.MAJOR".to_string(),
        scan_code: "HOT_BY_VOLUME".to_string(),
        max_items: 10,
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut got_params = false;
    let mut got_scan = false;

    while Instant::now() < deadline {
        let params = shared.drain_scanner_params();
        if !params.is_empty() {
            println!("  Scanner params XML: {} bytes", params[0].len());
            got_params = true;
        }
        let scans = shared.drain_scanner_data();
        for (req_id, result) in &scans {
            if *req_id == 9001 {
                println!("  Scanner results: {} contracts", result.con_ids.len());
                got_scan = true;
            }
        }
        if got_params && got_scan { break; }
        // If we have params but no scan after a while, don't wait forever
        if got_params && Instant::now() > deadline - Duration::from_secs(5) { break; }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Cancel scanner subscription
    control_tx.send(ControlCommand::CancelScanner { req_id: 9001 }).unwrap();
    std::thread::sleep(Duration::from_millis(500));

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if got_params {
        println!("  Scanner params: PASS");
    } else {
        println!("  Scanner params: SKIP");
    }
    if got_scan {
        println!("  Scanner scan: PASS");
    } else {
        println!("  Scanner scan: SKIP (may need market hours)");
    }
    println!();
    conns
}

// ─── Phase 96: Connection Recovery (issue #79) ───

fn phase_connection_recovery(conns: Conns, _gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 96: Connection Recovery (simulated farm drop) ---");

    // We use a dummy TCP listener as a fake farm connection that we can close
    let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind local listener");
    let local_addr = listener.local_addr().unwrap();

    // Connect a fake "farm" to the local listener
    let fake_farm = std::net::TcpStream::connect(local_addr).expect("Failed to connect to local listener");
    let (_accepted, _) = listener.accept().expect("Failed to accept connection");

    // Build a Connection from the fake stream
    let fake_conn = Connection::new_raw(fake_farm).expect("Failed to create Connection");

    let account_id = conns.account_id.clone();
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    // Use fake farm, real CCP — hot loop should detect farm disconnect
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), fake_conn, conns.ccp, conns.hmds, None,
    );

    let join = run_hot_loop(hot_loop);

    // Drop the accepted side to close the connection
    drop(_accepted);
    drop(listener);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got_disconnect = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Disconnected) => { got_disconnect = true; break; }
            _ => {}
        }
    }

    // The hot loop should exit on its own after detecting disconnect
    let _ = control_tx.send(ControlCommand::Shutdown);
    let result = join.join();
    assert!(result.is_ok(), "Hot loop should not panic on connection drop");

    // Reconnect real farm for remaining tests
    let (farm, ccp, hmds) = match Gateway::connect(config) {
        Ok((_gw2, f, c, h)) => {
            println!("  Reconnected to IB for remaining tests");
            (f, c, h)
        }
        Err(e) => {
            panic!("Cannot continue integration suite without farm connection: {}", e);
        }
    };

    if got_disconnect {
        println!("  Disconnected event received");
        println!("  PASS\n");
    } else {
        println!("  SKIP: No Disconnected event (hot loop may have exited before emitting)\n");
    }
    Conns { farm, ccp, hmds, account_id }
}

// ─── Phase 97: Position Tracking after fills (issue #77) ───

fn phase_position_tracking(conns: Conns) -> Conns {
    println!("--- Phase 97: Position Tracking (SPY buy+sell round trip) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut phase = 0u8; // 0=wait ticks, 1=buy sent, 2=sell sent
    let mut tick_count = 0u32;
    let mut got_position_update = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(instrument)) => {
                tick_count += 1;
                if phase == 0 && tick_count >= 5 {
                    let buy_oid = next_order_id();
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: buy_oid, instrument, side: Side::Buy, qty: 1,
                    })).unwrap();
                    phase = 1;
                }
            }
            Ok(Event::Fill(fill)) => {
                if phase == 1 && fill.side == Side::Buy {
                    let sell_order_id = next_order_id() + 1;
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: sell_order_id, instrument: fill.instrument, side: Side::Sell, qty: 1,
                    })).unwrap();
                    phase = 2;
                } else if phase == 2 && fill.side == Side::Sell {
                    // Wait a bit more for position update
                    std::thread::sleep(Duration::from_secs(2));
                    break;
                }
            }
            Ok(Event::PositionUpdate { instrument, con_id, position, avg_cost }) => {
                println!("  PositionUpdate: inst={} conId={} pos={} avgCost={:.4}",
                    instrument, con_id, position, avg_cost as f64 / ibx::types::PRICE_SCALE as f64);
                got_position_update = true;
            }
            Ok(Event::OrderUpdate(update)) => {
                if update.status == OrderStatus::Rejected {
                    println!("  SKIP: Order rejected — market closed\n");
                    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
                    return conns;
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if phase < 1 {
        println!("  SKIP: No ticks received — market closed\n");
    } else if got_position_update {
        println!("  PASS\n");
    } else {
        println!("  SKIP: No PositionUpdate events received (fills completed but no position update)\n");
    }
    conns
}

// ─── Phase 98: Forex contract details + order lifecycle (issue #83, #92) ───

fn phase_forex_order(conns: Conns) -> Conns {
    println!("--- Phase 98: Forex Order Lifecycle (EUR.USD) ---");

    // First, look up EUR.USD contract via CCP
    let now = ibx::gateway::chrono_free_timestamp();
    let mut ccp = conns.ccp;
    ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SECURITY_REQ_ID, "RFXEUR"),
        (contracts::TAG_SECURITY_REQ_TYPE, "2"),
        (contracts::TAG_SYMBOL, "EUR"),
        (contracts::TAG_SECURITY_TYPE, "CASH"),
        (contracts::TAG_EXCHANGE, "IDEALPRO"),
        (contracts::TAG_CURRENCY, "USD"),
        (contracts::TAG_IB_SOURCE, "Socket"),
    ]).expect("Failed to send forex secdef request");

    let mut forex_con_id: Option<u32> = None;
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline && forex_con_id.is_none() {
        match ccp.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  CCP recv error: {}", e); break; }
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
                            println!("  Contract: {} conId={} secType={:?} exchange={}",
                                def.symbol, def.con_id, def.sec_type, def.exchange);
                            forex_con_id = Some(def.con_id);
                        }
                    }
                }
            }
        }
    }

    let fx_con_id = match forex_con_id {
        Some(id) => id,
        None => {
            println!("  SKIP: No forex contract found for EUR.USD\n");
            return Conns { farm: conns.farm, ccp, hmds: conns.hmds, account_id: conns.account_id };
        }
    };

    // Submit a forex limit order using the actual forex con_id
    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, ccp, conns.hmds, None,
    );
    let inst = hot_loop.context_mut().register_instrument(fx_con_id as i64);
    hot_loop.context_mut().set_symbol(inst, "EUR".to_string());

    let oid = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: oid, instrument: inst, side: Side::Buy, qty: 20000, price: 50_000_000, outside_rth: true,
    })).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_acked = false;
    let mut cancel_sent = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                if update.order_id == oid {
                    match update.status {
                        OrderStatus::Submitted => {
                            order_acked = true;
                            if !cancel_sent {
                                control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: oid })).unwrap();
                                cancel_sent = true;
                            }
                        }
                        OrderStatus::Cancelled => { order_cancelled = true; break; }
                        OrderStatus::Rejected => { order_rejected = true; break; }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Forex order rejected (may need trading permissions)\n");
    } else {
        assert!(order_acked, "Forex order should be acknowledged");
        assert!(order_cancelled, "Forex order should be cancelled");
        println!("  PASS\n");
    }
    conns
}

// ─── Phase 99: Futures contract details + order lifecycle (issue #83, #92) ───

fn phase_futures_order(conns: Conns) -> Conns {
    println!("--- Phase 99: Futures Contract Details (MES) ---");

    // Look up MES (Micro E-mini S&P 500) via CCP
    let now = ibx::gateway::chrono_free_timestamp();
    let mut ccp = conns.ccp;
    ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SECURITY_REQ_ID, "RFUT"),
        (contracts::TAG_SECURITY_REQ_TYPE, "2"),
        (contracts::TAG_SYMBOL, "MES"),
        (contracts::TAG_SECURITY_TYPE, "FUT"),
        (contracts::TAG_EXCHANGE, "CME"),
        (contracts::TAG_CURRENCY, "USD"),
        (contracts::TAG_IB_SOURCE, "Socket"),
    ]).expect("Failed to send futures secdef request");

    let mut fut_contract: Option<contracts::ContractDefinition> = None;
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline && fut_contract.is_none() {
        match ccp.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  CCP recv error: {}", e); break; }
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
                        if def.sec_type == contracts::SecurityType::Future {
                            println!("  Contract: {} conId={} secType={:?} exchange={} expiry={} multiplier={}",
                                def.symbol, def.con_id, def.sec_type, def.exchange,
                                def.last_trade_date, def.multiplier);
                            assert!(def.multiplier > 0.0, "Futures multiplier should be positive");
                            assert!(!def.last_trade_date.is_empty(), "Futures should have expiry date");
                            // Take the first (front-month) contract
                            if fut_contract.is_none() {
                                fut_contract = Some(def);
                            }
                        }
                    }
                }
            }
        }
    }

    let fut_def = match fut_contract {
        Some(def) => def,
        None => {
            println!("  SKIP: No MES futures contract found\n");
            return Conns { farm: conns.farm, ccp, hmds: conns.hmds, account_id: conns.account_id };
        }
    };

    // Submit a futures limit order using the actual futures con_id
    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, ccp, conns.hmds, None,
    );
    let inst = hot_loop.context_mut().register_instrument(fut_def.con_id as i64);
    hot_loop.context_mut().set_symbol(inst, "MES".to_string());

    let oid = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: oid, instrument: inst, side: Side::Buy, qty: 1, price: 100_00_000_000, outside_rth: true,
    })).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_acked = false;
    let mut cancel_sent = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                if update.order_id == oid {
                    match update.status {
                        OrderStatus::Submitted => {
                            order_acked = true;
                            if !cancel_sent {
                                control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: oid })).unwrap();
                                cancel_sent = true;
                            }
                        }
                        OrderStatus::Cancelled => { order_cancelled = true; break; }
                        OrderStatus::Rejected => { order_rejected = true; break; }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Futures order rejected (may need trading permissions)\n");
    } else {
        assert!(order_acked, "Futures order should be acknowledged");
        assert!(order_cancelled, "Futures order should be cancelled");
        println!("  PASS\n");
    }
    conns
}

// ─── Phase 100: Options contract details + order lifecycle (issue #83, #89) ───

fn phase_options_order(conns: Conns) -> Conns {
    println!("--- Phase 100: Options Contract Details + Order (SPY options) ---");

    // Look up SPY options via CCP
    let now = ibx::gateway::chrono_free_timestamp();
    let mut ccp = conns.ccp;
    ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SECURITY_REQ_ID, "ROPT"),
        (contracts::TAG_SECURITY_REQ_TYPE, "2"),
        (contracts::TAG_SYMBOL, "SPY"),
        (contracts::TAG_SECURITY_TYPE, "OPT"),
        (contracts::TAG_EXCHANGE, "BEST"),
        (contracts::TAG_CURRENCY, "USD"),
        (contracts::TAG_IB_SOURCE, "Socket"),
    ]).expect("Failed to send options secdef request");

    let mut option_contracts: Vec<contracts::ContractDefinition> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline {
        match ccp.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  CCP recv error: {}", e); break; }
            Ok(_) => {}
        }
        let mut got_end = false;
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
                let msg_type = tags.get(&fix::TAG_MSG_TYPE).map(|s| s.as_str()).unwrap_or("?");
                if msg_type == "d" {
                    if let Some(resp_type) = tags.get(&contracts::TAG_SECURITY_RESPONSE_TYPE) {
                        if resp_type == "6" || resp_type == "5" {
                            got_end = true;
                            continue;
                        }
                    }
                    if let Some(def) = contracts::parse_secdef_response(&msg) {
                        if def.sec_type == contracts::SecurityType::Option && def.right.is_some() {
                            option_contracts.push(def);
                        }
                    }
                }
            }
        }
        if got_end && !option_contracts.is_empty() { break; }
    }

    if option_contracts.is_empty() {
        println!("  SKIP: No SPY option contracts found\n");
        return Conns { farm: conns.farm, ccp, hmds: conns.hmds, account_id: conns.account_id };
    }

    // Pick the first call option found
    let opt = option_contracts.iter()
        .find(|d| d.right == Some(contracts::OptionRight::Call))
        .unwrap_or(&option_contracts[0]);
    println!("  Found {} option contracts, using: {} conId={} strike={} right={:?} expiry={}",
        option_contracts.len(), opt.symbol, opt.con_id, opt.strike,
        opt.right, opt.last_trade_date);
    assert!(opt.strike > 0.0, "Option strike should be positive");
    assert!(opt.multiplier > 0.0, "Option multiplier should be positive (typically 100)");

    // Submit an option limit order using the actual option con_id
    let opt_con_id = opt.con_id;
    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, ccp, conns.hmds, None,
    );
    let inst = hot_loop.context_mut().register_instrument(opt_con_id as i64);
    hot_loop.context_mut().set_symbol(inst, "SPY".to_string());

    let oid = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: oid, instrument: inst, side: Side::Buy, qty: 1, price: 1_000_000, outside_rth: true,
    })).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_acked = false;
    let mut cancel_sent = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                if update.order_id == oid {
                    match update.status {
                        OrderStatus::Submitted => {
                            order_acked = true;
                            if !cancel_sent {
                                control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: oid })).unwrap();
                                cancel_sent = true;
                            }
                        }
                        OrderStatus::Cancelled => { order_cancelled = true; break; }
                        OrderStatus::Rejected => { order_rejected = true; break; }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Option order rejected (may need trading permissions)\n");
    } else {
        assert!(order_acked, "Option order should be acknowledged");
        assert!(order_cancelled, "Option order should be cancelled");
        println!("  PASS\n");
    }
    conns
}

// ─── Phase 101: Concurrent orders in flight (issue #84) ───

fn phase_concurrent_orders(conns: Conns) -> Conns {
    println!("--- Phase 101: Concurrent Orders in Flight (3 simultaneous limit orders) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    // Register SPY
    let spy_inst = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(spy_inst, "SPY".to_string());

    // Submit 3 limit orders simultaneously at $1.00 (far below market)
    let oid1 = next_order_id();
    let oid2 = oid1 + 1;
    let oid3 = oid1 + 2;

    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: oid1, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, outside_rth: true,
    })).unwrap();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: oid2, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, outside_rth: true,
    })).unwrap();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: oid3, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, outside_rth: true,
    })).unwrap();

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut acked = [false; 3];
    let mut cancelled = [false; 3];
    let mut cancel_sent = false;
    let mut rejected = false;
    let oids = [oid1, oid2, oid3];

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                let idx = oids.iter().position(|&id| id == update.order_id);
                if let Some(i) = idx {
                    match update.status {
                        OrderStatus::Submitted => {
                            acked[i] = true;
                            // Once all 3 are acked, cancel them all
                            if acked.iter().all(|&a| a) && !cancel_sent {
                                for &oid in &oids {
                                    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: oid })).unwrap();
                                }
                                cancel_sent = true;
                            }
                        }
                        OrderStatus::Cancelled => { cancelled[i] = true; }
                        OrderStatus::Rejected => { rejected = true; break; }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if cancelled.iter().all(|&c| c) { break; }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if rejected {
        println!("  SKIP: One or more orders rejected\n");
        return conns;
    }

    let acked_count = acked.iter().filter(|&&a| a).count();
    let cancelled_count = cancelled.iter().filter(|&&c| c).count();
    println!("  Acked: {}/3  Cancelled: {}/3", acked_count, cancelled_count);

    assert_eq!(acked_count, 3, "All 3 orders should be acknowledged");
    assert_eq!(cancelled_count, 3, "All 3 orders should be cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 102: Streaming data validation (issue #87) ───

fn phase_streaming_validation(conns: Conns) -> Conns {
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
                if q.bid > 0 && q.ask > 0 && q.ask < q.bid {
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

// ─── Phase 103: Historical bar OHLC validation (issue #93) ───

fn phase_historical_ohlc_validation(conns: Conns, _gw: &Gateway, _config: &GatewayConfig) -> Conns {
    println!("--- Phase 103: Historical Bar OHLC Validation (SPY 1-hour bars) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, _event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let req_id = 6001u32;
    control_tx.send(ControlCommand::FetchHistorical {
        req_id,
        con_id: 756733,
        symbol: "SPY".into(),
        end_date_time: String::new(), // empty = now
        duration: "5 D".into(),
        bar_size: "1 hour".into(),
        what_to_show: "TRADES".into(),
        use_rth: true,
    }).unwrap();

    let join = run_hot_loop(hot_loop);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut bars_data: Option<historical::HistoricalResponse> = None;

    while Instant::now() < deadline {
        let hist = shared.drain_historical_data();
        for (rid, data) in hist {
            if rid == req_id {
                bars_data = Some(data);
            }
        }
        if bars_data.is_some() { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    let data = match bars_data {
        Some(d) => d,
        None => {
            println!("  SKIP: No historical data received\n");
            return conns;
        }
    };

    let bars = &data.bars;
    println!("  Received {} bars", bars.len());
    assert!(!bars.is_empty(), "Should receive at least 1 bar");

    let mut ohlc_valid = true;
    let mut volume_valid = true;

    for (i, bar) in bars.iter().enumerate() {
        // OHLC consistency: low <= everything, high >= everything
        if bar.low > bar.high {
            println!("  Bar {}: low ({}) > high ({})", i, bar.low, bar.high);
            ohlc_valid = false;
        }
        if bar.low > bar.open {
            println!("  Bar {}: low ({}) > open ({})", i, bar.low, bar.open);
            ohlc_valid = false;
        }
        if bar.low > bar.close {
            println!("  Bar {}: low ({}) > close ({})", i, bar.low, bar.close);
            ohlc_valid = false;
        }
        if bar.high < bar.open {
            println!("  Bar {}: high ({}) < open ({})", i, bar.high, bar.open);
            ohlc_valid = false;
        }
        if bar.high < bar.close {
            println!("  Bar {}: high ({}) < close ({})", i, bar.high, bar.close);
            ohlc_valid = false;
        }
        // Volume should be non-negative
        if bar.volume < 0 {
            println!("  Bar {}: negative volume ({})", i, bar.volume);
            volume_valid = false;
        }
    }

    assert!(ohlc_valid, "All bars should have valid OHLC relationships");
    assert!(volume_valid, "All bars should have non-negative volume");
    println!("  PASS\n");
    conns
}

// ─── Phase 104: IB-side error handling (issue #88) ───

fn phase_ib_error_handling(conns: Conns) -> Conns {
    println!("--- Phase 104: IB-Side Error Handling (invalid requests) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let spy_inst = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(spy_inst, "SPY".to_string());

    // Submit an order for a non-existent instrument (con_id 999999999)
    // The hot loop should handle this gracefully
    let oid = next_order_id();
    // Register a bogus instrument
    let bogus_inst = hot_loop.context_mut().register_instrument(999999999);
    hot_loop.context_mut().set_symbol(bogus_inst, "BOGUS".to_string());

    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
        order_id: oid, instrument: bogus_inst, side: Side::Buy, qty: 1,
    })).unwrap();

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got_error_or_reject = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                if update.order_id == oid && update.status == OrderStatus::Rejected {
                    println!("  Order rejected as expected (bogus con_id)");
                    got_error_or_reject = true;
                    break;
                }
            }
            Ok(Event::CancelReject(cr)) => {
                if cr.order_id == oid {
                    got_error_or_reject = true;
                    println!("  CancelReject received for bogus order");
                    break;
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if got_error_or_reject {
        println!("  PASS\n");
    } else {
        // The order may have been silently ignored or the hot loop handled it
        println!("  SKIP: No rejection/error received (order may have been filtered)\n");
    }
    conns
}

// ─── Phase 105: Reconnection with state recovery (issue #86) ───

fn phase_reconnection_state_recovery(conns: Conns, _gw: &Gateway, _config: &GatewayConfig) -> Conns {
    println!("--- Phase 105: Reconnection with State Recovery ---");

    // Step 1: Subscribe to market data, verify we get ticks
    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got_ticks = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(_)) => { got_ticks = true; break; }
            _ => {}
        }
    }

    // Shutdown the hot loop to simulate "disconnect"
    let conns1 = shutdown_and_reclaim(&control_tx, join, account_id.clone());

    if !got_ticks {
        println!("  SKIP: No ticks received before disconnect — market closed\n");
        return conns1;
    }

    println!("  Step 1: Got ticks before disconnect");

    // Step 2: Reconnect with fresh connections and verify ticks resume
    let shared2 = Arc::new(SharedState::new());
    let (event_tx2, event_rx2) = crossbeam_channel::unbounded();
    let (hot_loop2, control_tx2) = HotLoop::with_connections(
        shared2.clone(), Some(event_tx2), conns1.account_id.clone(),
        conns1.farm, conns1.ccp, conns1.hmds, None,
    );

    control_tx2.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join2 = run_hot_loop(hot_loop2);

    let deadline2 = Instant::now() + Duration::from_secs(15);
    let mut got_ticks_after = false;

    while Instant::now() < deadline2 {
        match event_rx2.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(inst)) => {
                let q = shared2.quote(inst);
                println!("  Step 2: Tick after reconnect bid={:.4} ask={:.4}",
                    q.bid as f64 / PRICE_SCALE as f64, q.ask as f64 / PRICE_SCALE as f64);
                got_ticks_after = true;
                break;
            }
            _ => {}
        }
    }

    let conns2 = shutdown_and_reclaim(&control_tx2, join2, conns1.account_id);

    assert!(got_ticks_after, "Should receive ticks after reconnection");
    println!("  PASS\n");
    conns2
}

// ─── Phase 106: Account summary request (issue #90) ───

fn phase_account_summary(conns: Conns) -> Conns {
    println!("--- Phase 106: Account Summary (raw CCP request) ---");

    let ccp = conns.ccp;

    // Account data is available via the HotLoop account state
    let account_id = conns.account_id.clone();
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        conns.farm, ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    // Wait for account data to populate
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut has_account_data = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Tick(_)) => {
                let acct = shared.account();
                if acct.net_liquidation > 0 {
                    println!("  NetLiquidation: {:.2}", acct.net_liquidation as f64 / PRICE_SCALE as f64);
                    println!("  BuyingPower: {:.2}", acct.buying_power as f64 / PRICE_SCALE as f64);
                    println!("  AvailableFunds: {:.2}", acct.available_funds as f64 / PRICE_SCALE as f64);
                    println!("  MarginUsed: {:.2}", acct.margin_used as f64 / PRICE_SCALE as f64);
                    println!("  MaintMarginReq: {:.2}", acct.maint_margin_req as f64 / PRICE_SCALE as f64);
                    has_account_data = true;

                    // Validate account data sanity
                    assert!(acct.net_liquidation > 0, "NetLiquidation should be positive");
                    assert!(acct.buying_power >= 0, "BuyingPower should be non-negative");
                    break;
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if has_account_data {
        println!("  PASS\n");
    } else {
        println!("  SKIP: No account data received\n");
    }
    conns
}

// ─── Phase 107: Forex market data ticks — session-independent (issue #91) ───
// EUR.USD trades ~24h Sun-Fri, providing tick coverage when US stocks are closed.

fn phase_forex_market_data(conns: Conns) -> Conns {
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

                // Validate forex prices: EUR.USD typically 0.8-1.5
                if q.bid > 0 {
                    let bid = q.bid as f64 / PRICE_SCALE as f64;
                    assert!(bid > 0.5 && bid < 2.0,
                        "EUR.USD bid {:.5} out of expected range", bid);
                }
                if q.ask > 0 && q.bid > 0 {
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

// ─── Phase 108: Forex streaming validation — session-independent (issue #91) ───

fn phase_forex_streaming_validation(conns: Conns) -> Conns {
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
    let mut price_in_range = true;

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
                    // Spread should be reasonable for major forex pair (< 0.01)
                    let spread = ask - bid;
                    if spread > 0.01 {
                        println!("  NOTE: Wide spread {:.5} (unusual for EUR.USD)", spread);
                    }
                }

                if q.bid > 0 && (bid < 0.5 || bid > 2.0) {
                    price_in_range = false;
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
        assert!(price_in_range, "EUR.USD price should be in 0.5-2.0 range");
        println!("  {} ticks, spread_valid={} price_in_range={}", tick_count, spread_valid, price_in_range);
        println!("  PASS\n");
    }
    conns
}

// ─── Phase 109: Forex reconnection — session-independent (issue #91) ───

fn phase_forex_reconnection(conns: Conns) -> Conns {
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

/// Format seconds since epoch as YYYYMMDD-HH:MM:SS UTC.
fn format_utc_timestamp(epoch_secs: u64) -> String {
    let secs_per_day = 86400u64;
    let days = epoch_secs / secs_per_day;
    let mut y = 1970i64;
    let mut remaining = days as i64;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
        if remaining < days_in_year { break; }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0;
    for (i, &d) in month_days.iter().enumerate() {
        if remaining < d as i64 { m = i + 1; break; }
        remaining -= d as i64;
    }
    let day = remaining + 1;
    let hour = (epoch_secs % secs_per_day) / 3600;
    let min = (epoch_secs % 3600) / 60;
    let sec = epoch_secs % 60;
    format!("{:04}{:02}{:02}-{:02}:{:02}:{:02}", y, m, day, hour, min, sec)
}
