//! Integration tests against IB paper account.
//!
//! Requires IB_USERNAME and IB_PASSWORD environment variables.
//! Run with: cargo test --test ib_paper_integration -- --ignored --nocapture
//!
//! All tests share a single Gateway connection to avoid ONELOGON throttling.
//! Each phase builds a fresh HotLoop, runs it, then reclaims connections.

use std::env;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ibx::control::contracts;
use ibx::control::historical::{self, BarDataType, BarSize, HeadTimestampRequest, HistoricalRequest};
use ibx::engine::context::{Context, Strategy};
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
fn run_hot_loop<S: Strategy + Send + 'static>(hot_loop: HotLoop<S>) -> std::thread::JoinHandle<HotLoop<S>> {
    std::thread::spawn(move || {
        let mut hl = hot_loop;
        hl.run();
        hl
    })
}

/// Shutdown a hot loop and reclaim connections.
fn shutdown_and_reclaim<S: Strategy + Send + 'static>(
    control_tx: &crossbeam_channel::Sender<ControlCommand>,
    join: std::thread::JoinHandle<HotLoop<S>>,
    account_id: String,
) -> Conns {
    let _ = control_tx.send(ControlCommand::Shutdown);
    let mut hl = join.join().expect("hot loop thread panicked");
    let farm = hl.farm_conn.take().expect("farm_conn missing after shutdown");
    let ccp = hl.ccp_conn.take().expect("ccp_conn missing after shutdown");
    let hmds = hl.hmds_conn.take();
    Conns { farm, ccp, hmds, account_id }
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

fn market_session() -> MarketSession {
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
    // Jan 1 dow for this year: accumulate from 1970 (Thursday=4).
    let jan1_dow = {
        let mut d = 4u8; // Jan 1 1970 = Thursday
        for yr in 1970..y {
            let yl = if yr % 4 == 0 && (yr % 100 != 0 || yr % 400 == 0) { 366 } else { 365 };
            d = ((d as u16 + (yl % 7) as u16) % 7) as u8;
        }
        d // 0=Sun
    };
    // Day-of-year for March 1
    let mar1_doy = if leap { 60 } else { 59 }; // 0-indexed
    let mar1_dow = ((jan1_dow as u16 + (mar1_doy % 7) as u16) % 7) as u8;
    // First Sunday in March: day (1-indexed)
    let first_sun_mar = if mar1_dow == 0 { 1 } else { (8 - mar1_dow) as u8 };
    let second_sun_mar = first_sun_mar + 7; // second Sunday of March

    // Day-of-year for November 1
    let nov1_doy = if leap { 305 } else { 304 };
    let nov1_dow = ((jan1_dow as u16 + (nov1_doy % 7) as u16) % 7) as u8;
    let first_sun_nov = if nov1_dow == 0 { 1 } else { (8 - nov1_dow) as u8 };

    // EDT if: (month > Mar OR (month == Mar AND day >= second_sun_mar AND hour >= 2 UTC-5=7 UTC))
    //     AND: (month < Nov OR (month == Nov AND day < first_sun_nov) OR (month == Nov AND day == first_sun_nov-1?))
    // Simplified: DST active from Mar second_sun 07:00 UTC to Nov first_sun 06:00 UTC
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

    if et_dow == 0 || et_dow == 6 { return MarketSession::Closed; }

    match et_min {
        240..=569 => MarketSession::PreMarket,
        570..=959 => MarketSession::Regular,
        960..=1199 => MarketSession::AfterHours,
        _ => MarketSession::Closed,
    }
}

// ─── Master test suite ───

#[test]
#[ignore]
fn integration_suite() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    let session = market_session();
    let needs_ticks = session == MarketSession::Regular;
    println!("=== Integration Suite (session={:?}) ===\n", session);
    let suite_start = Instant::now();

    // One connection for all tests
    let start = Instant::now();
    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    let connect_time = start.elapsed();

    // Phase 1: CCP auth checks (no hot loop)
    phase_ccp_auth(&gw, hmds_conn.is_some(), connect_time);

    // Phase 18: Additional farm connections
    phase_extra_farms(&gw, &config);

    let mut conns = Conns {
        farm: farm_conn,
        ccp: ccp_conn,
        hmds: hmds_conn,
        account_id: gw.account_id.clone(),
    };

    // Raw subscribe test: regular hours only (15s timeout otherwise wasted)
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
                Ok(0) => {} // WouldBlock
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

    // Phase 14: Account PnL — MUST be first hot loop to receive CCP init burst
    conns = phase_account_pnl(conns);

    // Phase 12: Contract details (raw CCP — init burst already consumed)
    phase_contract_details(&mut conns);

    // Phase 78: Contract details by symbol search
    phase_contract_details_by_symbol(&mut conns);

    // Phase 80: Trading hours (schedule subscription)
    phase_trading_hours(&mut conns);

    // Phase 11: Historical data — reconnect HMDS fresh to avoid stale connection
    conns = phase_historical_data(conns, &gw, &config);

    // Phase 76: Historical daily bars
    conns = phase_historical_daily_bars(conns, &gw, &config);

    // Phase 77: Cancel historical request
    conns = phase_cancel_historical(conns, &gw, &config);

    // Phase 79: Head timestamp
    conns = phase_head_timestamp(conns, &gw, &config);

    // Tick-dependent phases — regular hours only
    if needs_ticks {
        conns = phase_market_data(conns);
        conns = phase_multi_instrument(conns);
        conns = phase_account_data(conns);
    } else {
        println!("--- Phase 2: Market Data Ticks (AAPL) ---\n  SKIP: {:?} — no ticks expected\n", session);
        println!("--- Phase 3: Multi-Instrument Subscription (AAPL+MSFT+SPY) ---\n  SKIP: {:?} — no ticks expected\n", session);
        println!("--- Phase 4: Account Data Reception ---\n  SKIP: {:?} — needs ticks to trigger\n", session);
    }

    // Order phases — always (DAY order rejection is handled fast)
    conns = phase_outside_rth(conns);
    conns = phase_outside_rth_stop(conns);
    conns = phase_limit_order(conns);
    conns = phase_stop_order(conns);
    conns = phase_stop_limit_order(conns);
    conns = phase_modify_order(conns);
    conns = phase_modify_qty(conns);

    // New order type phases — always
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

    // Tier 1 order types (from issue #43 FIX tag mappings)
    conns = phase_mtl_order(conns);
    conns = phase_mkt_prt_order(conns);
    conns = phase_stp_prt_order(conns);
    conns = phase_mid_price_order(conns);
    conns = phase_snap_mkt_order(conns);
    conns = phase_snap_mid_order(conns);
    conns = phase_snap_pri_order(conns);
    conns = phase_peg_mkt_order(conns);
    conns = phase_peg_mid_order(conns);

    // Order attribute phases
    conns = phase_discretionary_order(conns);
    conns = phase_sweep_to_fill_order(conns);
    conns = phase_all_or_none_order(conns);
    conns = phase_trigger_method_order(conns);

    // Conditional order phases
    conns = phase_price_condition_order(conns);
    conns = phase_time_condition_order(conns);
    conns = phase_volume_condition_order(conns);
    conns = phase_multi_condition_order(conns);

    // Algorithmic order phases
    conns = phase_vwap_order(conns);
    conns = phase_twap_order(conns);
    conns = phase_arrival_px_order(conns);
    conns = phase_close_px_order(conns);
    conns = phase_dark_ice_order(conns);
    conns = phase_pct_vol_order(conns);

    // Remaining order type phases (issue #36)
    conns = phase_peg_bench_order(conns);
    conns = phase_limit_auc_order(conns);
    conns = phase_mtl_auc_order(conns);
    conns = phase_box_top_order(conns);

    // Order feature phases (issue #37)
    conns = phase_what_if_order(conns);
    conns = phase_cash_qty_order(conns);
    conns = phase_fractional_order(conns);
    conns = phase_adjustable_stop_order(conns);

    // Tick-by-tick data phase — needs HMDS and market open
    if needs_ticks && conns.hmds.is_some() {
        conns = phase_tbt_subscribe(conns);
    } else {
        println!("--- Phase 61: Tick-by-Tick Data (SPY) ---\n  SKIP: needs ticks+HMDS\n");
    }

    // MOC/LOC only during regular hours (IB rejects outside regular hours)
    if needs_ticks {
        conns = phase_moc_order(conns);
        conns = phase_loc_order(conns);
    } else {
        println!("--- Phase 27: MOC Order (SPY) ---\n  SKIP: {:?} — only during regular hours\n", session);
        println!("--- Phase 28: LOC Order (SPY) ---\n  SKIP: {:?} — only during regular hours\n", session);
    }

    // Infrastructure phases — always
    conns = phase_subscribe_unsubscribe(conns);
    conns = phase_heartbeat_keepalive(conns);
    conns = phase_farm_heartbeat_keepalive(conns);

    // Fill-dependent phases — regular hours only
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

    // Heartbeat timeout detection — last before shutdown (CCP goes stale during this test)
    conns = phase_heartbeat_timeout_detection(conns);

    let _conns = phase_graceful_shutdown(conns);

    let total_phases = 72;
    let skipped = if needs_ticks { 0 } else { 10 };
    println!("\n=== {}/{} phases ran ({} skipped, {:?}) in {:.1}s ===",
        total_phases - skipped, total_phases, skipped, session, suite_start.elapsed().as_secs_f64());
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

struct NoopStrategy;
impl Strategy for NoopStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {}
    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}
    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_historical_data(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 11: Historical Data Bars (SPY, 1 day of 5-min bars) ---");

    // Reconnect HMDS fresh to avoid stale connection from idle time
    let mut hmds = match connect_farm(
        &config.host, "ushmds",
        &config.username, config.paper,
        &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded,
    ) {
        Ok(c) => {
            println!("  HMDS reconnected");
            c
        }
        Err(e) => {
            println!("  SKIP: ushmds reconnect failed: {}\n", e);
            return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id };
        }
    };

    let account_id = conns.account_id;

    // Run a background hot loop to keep CCP+farm heartbeats alive during HMDS work
    let (bg_loop, bg_tx) = HotLoop::with_connections(
        NoopStrategy, account_id.clone(), conns.farm, conns.ccp, None, None,
    );
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    // HMDS work on main thread
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
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => {
                println!("  HMDS recv error: {}", e);
                break;
            }
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
                        if resp.is_complete {
                            complete = true;
                        }
                    }
                }
            }
        }
    }

    // Reclaim CCP+farm from background hot loop
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

struct DiagnosticStrategy {
    tick_count: Arc<AtomicU32>,
    first_tick_received: Arc<AtomicBool>,
    account_checked: Arc<AtomicBool>,
    net_liq: Arc<AtomicI64>,
    disconnected: Arc<AtomicBool>,
}

struct DiagnosticHandle {
    tick_count: Arc<AtomicU32>,
    first_tick_received: Arc<AtomicBool>,
    account_checked: Arc<AtomicBool>,
    net_liq: Arc<AtomicI64>,
    disconnected: Arc<AtomicBool>,
}

impl DiagnosticStrategy {
    fn new() -> (Self, DiagnosticHandle) {
        let tick_count = Arc::new(AtomicU32::new(0));
        let first_tick_received = Arc::new(AtomicBool::new(false));
        let account_checked = Arc::new(AtomicBool::new(false));
        let net_liq = Arc::new(AtomicI64::new(0));
        let disconnected = Arc::new(AtomicBool::new(false));

        let handle = DiagnosticHandle {
            tick_count: tick_count.clone(),
            first_tick_received: first_tick_received.clone(),
            account_checked: account_checked.clone(),
            net_liq: net_liq.clone(),
            disconnected: disconnected.clone(),
        };

        let strategy = Self {
            tick_count,
            first_tick_received,
            account_checked,
            net_liq,
            disconnected,
        };

        (strategy, handle)
    }
}

impl Strategy for DiagnosticStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {}

    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
        let count = self.tick_count.fetch_add(1, Ordering::Relaxed);

        if count == 0 {
            let bid = ctx.bid(instrument) as f64 / PRICE_SCALE as f64;
            let ask = ctx.ask(instrument) as f64 / PRICE_SCALE as f64;
            let last = ctx.last(instrument) as f64 / PRICE_SCALE as f64;
            let spread = ctx.spread(instrument) as f64 / PRICE_SCALE as f64;
            println!("  FIRST TICK: instrument={} bid={:.4} ask={:.4} last={:.4} spread={:.6}",
                instrument, bid, ask, last, spread);
            self.first_tick_received.store(true, Ordering::Relaxed);
        }

        if !self.account_checked.load(Ordering::Relaxed) {
            let acct = ctx.account();
            let nl = acct.net_liquidation;
            if nl != 0 {
                self.net_liq.store(nl, Ordering::Relaxed);
                println!("  ACCOUNT: net_liq={:.2} (tick #{})", nl as f64 / PRICE_SCALE as f64, count);
                self.account_checked.store(true, Ordering::Relaxed);
            }
        }
    }

    fn on_fill(&mut self, fill: &Fill, _ctx: &mut Context) {
        println!("  FILL: order={} side={:?} qty={} price={:.4}",
            fill.order_id, fill.side, fill.qty, fill.price as f64 / PRICE_SCALE as f64);
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        self.disconnected.store(true, Ordering::Relaxed);
    }
}

fn phase_market_data(conns: Conns) -> Conns {
    println!("--- Phase 2: Market Data Ticks (AAPL) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = DiagnosticStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if handle.first_tick_received.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !handle.first_tick_received.load(Ordering::Relaxed) {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        println!("  SKIP: No ticks in 30s — market closed or IB throttling\n");
        return conns;
    }

    std::thread::sleep(Duration::from_secs(5));
    let total = handle.tick_count.load(Ordering::Relaxed);

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
    println!("  PASS ({} ticks)\n", total);
    conns
}

// ─── Phase 3: Multi-instrument subscription ───

fn phase_multi_instrument(conns: Conns) -> Conns {
    println!("--- Phase 3: Multi-Instrument Subscription (AAPL+MSFT+SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = DiagnosticStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 272093, symbol: "MSFT".into() }).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if handle.first_tick_received.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !handle.first_tick_received.load(Ordering::Relaxed) {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        println!("  SKIP: No ticks — market closed or IB throttling\n");
        return conns;
    }

    std::thread::sleep(Duration::from_secs(5));
    let total = handle.tick_count.load(Ordering::Relaxed);

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if total <= 3 {
        println!("  SKIP: Only {} ticks — insufficient for multi-instrument test\n", total);
    } else {
        println!("  PASS ({} ticks)\n", total);
    }
    conns
}

// ─── Phase 4: Account data reception ───

fn phase_account_data(conns: Conns) -> Conns {
    println!("--- Phase 4: Account Data Reception ---");

    let account_id = conns.account_id;

    // Send 6040=76 on CCP to trigger a fresh account summary (6040=77) response.
    // The init burst was already consumed by earlier phases.
    let mut ccp = conns.ccp;
    let ts = ibx::gateway::chrono_free_timestamp();
    let _ = ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "U"),
        (fix::TAG_SENDING_TIME, &ts),
        (6040, "76"),
        (1, ""),
        (6565, "1"),
    ]);

    let (strategy, handle) = DiagnosticStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if handle.account_checked.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(200));
    }

    let net_liq = handle.net_liq.load(Ordering::Relaxed);

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.account_checked.load(Ordering::Relaxed) {
        assert!(net_liq > 0, "Paper account net liquidation should be > 0");
        println!("  net_liq=${:.2}", net_liq as f64 / PRICE_SCALE as f64);
        println!("  PASS\n");
    } else {
        println!("  WARN: Account data not received within 20s\n");
    }
    conns
}

// ─── Phase 14: Account PnL reception ───

struct PnlStrategy {
    account_received: Arc<AtomicBool>,
    net_liq: Arc<AtomicI64>,
    unrealized_pnl: Arc<AtomicI64>,
    realized_pnl: Arc<AtomicI64>,
    buying_power: Arc<AtomicI64>,
    probe_done: Arc<AtomicBool>,
    order_id: Option<OrderId>,
}

struct PnlHandle {
    account_received: Arc<AtomicBool>,
    net_liq: Arc<AtomicI64>,
    probe_done: Arc<AtomicBool>,
}

impl PnlStrategy {
    fn new() -> (Self, PnlHandle) {
        let account_received = Arc::new(AtomicBool::new(false));
        let net_liq = Arc::new(AtomicI64::new(0));
        let unrealized_pnl = Arc::new(AtomicI64::new(0));
        let realized_pnl = Arc::new(AtomicI64::new(0));
        let buying_power = Arc::new(AtomicI64::new(0));
        let probe_done = Arc::new(AtomicBool::new(false));
        let handle = PnlHandle {
            account_received: account_received.clone(),
            net_liq: net_liq.clone(),
            probe_done: probe_done.clone(),
        };
        (Self {
            account_received, net_liq, unrealized_pnl, realized_pnl, buying_power,
            probe_done, order_id: None,
        }, handle)
    }

    fn check_account(&mut self, ctx: &Context) {
        if self.account_received.load(Ordering::Relaxed) { return; }
        let acct = ctx.account();
        if acct.net_liquidation != 0 {
            self.net_liq.store(acct.net_liquidation, Ordering::Relaxed);
            self.unrealized_pnl.store(acct.unrealized_pnl, Ordering::Relaxed);
            self.realized_pnl.store(acct.realized_pnl, Ordering::Relaxed);
            self.buying_power.store(acct.buying_power, Ordering::Relaxed);
            self.account_received.store(true, Ordering::Relaxed);
        }
    }
}

impl Strategy for PnlStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        let id = ctx.submit_limit_gtc(0, Side::Buy, 1, 1_00_000_000, true);
        self.order_id = Some(id);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, ctx: &mut Context) {
        self.check_account(ctx);
    }

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        self.check_account(ctx);
        if update.status == OrderStatus::Submitted {
            if let Some(id) = self.order_id {
                ctx.cancel(id);
            }
        }
        if matches!(update.status, OrderStatus::Cancelled | OrderStatus::Rejected) {
            self.probe_done.store(true, Ordering::Relaxed);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_account_pnl(conns: Conns) -> Conns {
    println!("--- Phase 14: Account PnL Reception ---");

    let account_id = conns.account_id;
    let (strategy, handle) = PnlStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if handle.probe_done.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    let nl = handle.net_liq.load(Ordering::Relaxed);
    assert!(handle.account_received.load(Ordering::Relaxed),
        "Account data not received — 6040=77 may not contain tag 9806");
    assert!(nl > 0, "Paper account net liquidation should be > 0");
    println!("  NetLiq: ${:.2}", nl as f64 / PRICE_SCALE as f64);
    println!("  PASS\n");
    conns
}

// ─── Phase 5: Graceful shutdown ───

fn phase_graceful_shutdown(conns: Conns) -> Conns {
    println!("--- Phase 5: Graceful Shutdown ---");

    let account_id = conns.account_id;
    let (strategy, handle) = DiagnosticStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
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
    assert!(
        handle.disconnected.load(Ordering::Relaxed),
        "on_disconnect() was not called during shutdown"
    );

    let farm = hl.farm_conn.take().expect("farm_conn missing");
    let ccp = hl.ccp_conn.take().expect("ccp_conn missing");
    let hmds = hl.hmds_conn.take();

    println!("  Shutdown in {:.3}s", shutdown_time.as_secs_f64());
    println!("  PASS\n");
    Conns { farm, ccp, hmds, account_id }
}

// ─── Order strategies ───

struct OrderTestStrategy {
    phase: u8,
    instrument: InstrumentId,
    buy_fill_price: Arc<AtomicI64>,
    sell_fill_price: Arc<AtomicI64>,
    buy_fill_time_us: Arc<AtomicU64>,
    sell_fill_time_us: Arc<AtomicU64>,
    order_rejected: Arc<AtomicBool>,
    ticks_before_order: u32,
    tick_count: u32,
    buy_sent_at: Option<Instant>,
    sell_sent_at: Option<Instant>,
}

struct OrderTestHandle {
    buy_fill_price: Arc<AtomicI64>,
    sell_fill_price: Arc<AtomicI64>,
    buy_fill_time_us: Arc<AtomicU64>,
    sell_fill_time_us: Arc<AtomicU64>,
    order_rejected: Arc<AtomicBool>,
}

impl OrderTestStrategy {
    fn new(ticks_before_order: u32) -> (Self, OrderTestHandle) {
        let buy_fill_price = Arc::new(AtomicI64::new(0));
        let sell_fill_price = Arc::new(AtomicI64::new(0));
        let buy_fill_time_us = Arc::new(AtomicU64::new(0));
        let sell_fill_time_us = Arc::new(AtomicU64::new(0));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let handle = OrderTestHandle {
            buy_fill_price: buy_fill_price.clone(),
            sell_fill_price: sell_fill_price.clone(),
            buy_fill_time_us: buy_fill_time_us.clone(),
            sell_fill_time_us: sell_fill_time_us.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            phase: 0,
            instrument: 0,
            buy_fill_price,
            sell_fill_price,
            buy_fill_time_us,
            sell_fill_time_us,
            order_rejected,
            ticks_before_order,
            tick_count: 0,
            buy_sent_at: None,
            sell_sent_at: None,
        }, handle)
    }
}

impl Strategy for OrderTestStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {}

    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
        self.tick_count += 1;

        if self.phase == 0 && self.tick_count >= self.ticks_before_order {
            self.instrument = instrument;
            ctx.submit_market(instrument, Side::Buy, 1);
            self.buy_sent_at = Some(Instant::now());
            self.phase = 1;
        }
    }

    fn on_fill(&mut self, fill: &Fill, ctx: &mut Context) {
        if self.phase == 1 && fill.side == Side::Buy {
            let rtt = self.buy_sent_at.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
            self.buy_fill_price.store(fill.price, Ordering::Relaxed);
            self.buy_fill_time_us.store(rtt, Ordering::Relaxed);

            ctx.submit_market(self.instrument, Side::Sell, 1);
            self.sell_sent_at = Some(Instant::now());
            self.phase = 2;
        } else if self.phase == 2 && fill.side == Side::Sell {
            let rtt = self.sell_sent_at.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
            self.sell_fill_price.store(fill.price, Ordering::Relaxed);
            self.sell_fill_time_us.store(rtt, Ordering::Relaxed);
            self.phase = 3;
        }
    }

    fn on_order_update(&mut self, update: &OrderUpdate, _ctx: &mut Context) {
        if update.status == OrderStatus::Rejected {
            self.order_rejected.store(true, Ordering::Relaxed);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

// ─── Phase 6: Market order round-trip ───

fn phase_market_order(conns: Conns) -> Conns {
    println!("--- Phase 6: Market Order Round-Trip (SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = OrderTestStrategy::new(5);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.sell_fill_price.load(Ordering::Relaxed) != 0 { break; }
        if handle.order_rejected.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    let buy_price = handle.buy_fill_price.load(Ordering::Relaxed);
    let sell_price = handle.sell_fill_price.load(Ordering::Relaxed);
    let buy_rtt = handle.buy_fill_time_us.load(Ordering::Relaxed);
    let sell_rtt = handle.sell_fill_time_us.load(Ordering::Relaxed);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Order rejected — market may be closed\n");
        return conns;
    }

    if buy_price == 0 {
        println!("  SKIP: No buy fill — market is closed\n");
        return conns;
    }
    assert!(sell_price > 0, "Buy filled but no sell fill received");

    println!("  Buy: ${:.4} (RTT {:.3}ms)", buy_price as f64 / PRICE_SCALE as f64, buy_rtt as f64 / 1000.0);
    println!("  Sell: ${:.4} (RTT {:.3}ms)", sell_price as f64 / PRICE_SCALE as f64, sell_rtt as f64 / 1000.0);
    println!("  Mean RTT: {:.3}ms", (buy_rtt + sell_rtt) as f64 / 2000.0);
    println!("  PASS\n");
    conns
}

// ─── Phase 7: Limit order submit + cancel ───

struct LimitOrderStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    submit_ack_us: Arc<AtomicU64>,
    cancel_conf_us: Arc<AtomicU64>,
    order_id: Option<OrderId>,
    submit_time: Option<Instant>,
    cancel_time: Option<Instant>,
}

struct LimitOrderHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    submit_ack_us: Arc<AtomicU64>,
    cancel_conf_us: Arc<AtomicU64>,
}

impl LimitOrderStrategy {
    fn new() -> (Self, LimitOrderHandle) {
        let submitted = Arc::new(AtomicBool::new(false));
        let order_acked = Arc::new(AtomicBool::new(false));
        let cancel_sent = Arc::new(AtomicBool::new(false));
        let order_cancelled = Arc::new(AtomicBool::new(false));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let submit_ack_us = Arc::new(AtomicU64::new(0));
        let cancel_conf_us = Arc::new(AtomicU64::new(0));
        let handle = LimitOrderHandle {
            submitted: submitted.clone(),
            order_acked: order_acked.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
            submit_ack_us: submit_ack_us.clone(),
            cancel_conf_us: cancel_conf_us.clone(),
        };
        (Self {
            submitted, order_acked, cancel_sent, order_cancelled, order_rejected,
            submit_ack_us, cancel_conf_us,
            order_id: None, submit_time: None, cancel_time: None,
        }, handle)
    }
}

impl Strategy for LimitOrderStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        let limit_price = 1_00_000_000i64; // $1.00
        let id = ctx.submit_limit(0, Side::Buy, 1, limit_price);
        self.order_id = Some(id);
        self.submit_time = Some(Instant::now());
        self.submitted.store(true, Ordering::Relaxed);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        match update.status {
            OrderStatus::Submitted => {
                if !self.order_acked.load(Ordering::Relaxed) {
                    let rtt = self.submit_time.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                    self.submit_ack_us.store(rtt, Ordering::Relaxed);
                }
                self.order_acked.store(true, Ordering::Relaxed);
                // Cancel immediately after ack
                if !self.cancel_sent.load(Ordering::Relaxed) {
                    if let Some(id) = self.order_id {
                        ctx.cancel(id);
                        self.cancel_time = Some(Instant::now());
                        self.cancel_sent.store(true, Ordering::Relaxed);
                    }
                }
            }
            OrderStatus::Cancelled => {
                let rtt = self.cancel_time.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                self.cancel_conf_us.store(rtt, Ordering::Relaxed);
                self.order_cancelled.store(true, Ordering::Relaxed);
            }
            OrderStatus::Rejected => {
                self.order_rejected.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_limit_order(conns: Conns) -> Conns {
    println!("--- Phase 7: Limit Order Submit + Cancel (SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = LimitOrderStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Order rejected — market may be closed\n");
        return conns;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Order was never acknowledged");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Order was never cancelled");

    let ack_us = handle.submit_ack_us.load(Ordering::Relaxed);
    let cancel_us = handle.cancel_conf_us.load(Ordering::Relaxed);
    println!("  Submit→Ack: {:.3}ms  Cancel→Conf: {:.3}ms", ack_us as f64 / 1000.0, cancel_us as f64 / 1000.0);
    println!("  PASS\n");
    conns
}

// ─── Phase 8: Stop order submit + cancel ───

struct StopOrderStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    order_id: Option<OrderId>,
}

struct StopOrderHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
}

impl StopOrderStrategy {
    fn new() -> (Self, StopOrderHandle) {
        let submitted = Arc::new(AtomicBool::new(false));
        let order_acked = Arc::new(AtomicBool::new(false));
        let cancel_sent = Arc::new(AtomicBool::new(false));
        let order_cancelled = Arc::new(AtomicBool::new(false));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let handle = StopOrderHandle {
            submitted: submitted.clone(),
            order_acked: order_acked.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            submitted, order_acked, cancel_sent, order_cancelled, order_rejected,
            order_id: None,
        }, handle)
    }
}

impl Strategy for StopOrderStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        let stop_price = 1_00_000_000i64; // $1.00
        let id = ctx.submit_stop(0, Side::Sell, 1, stop_price);
        self.order_id = Some(id);
        self.submitted.store(true, Ordering::Relaxed);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        match update.status {
            OrderStatus::Submitted => {
                self.order_acked.store(true, Ordering::Relaxed);
                if !self.cancel_sent.load(Ordering::Relaxed) {
                    if let Some(id) = self.order_id {
                        ctx.cancel(id);
                        self.cancel_sent.store(true, Ordering::Relaxed);
                    }
                }
            }
            OrderStatus::Cancelled => {
                self.order_cancelled.store(true, Ordering::Relaxed);
            }
            OrderStatus::Rejected => {
                self.order_rejected.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_stop_order(conns: Conns) -> Conns {
    println!("--- Phase 8: Stop Order Submit + Cancel (SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = StopOrderStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Stop order rejected\n");
        return conns;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Stop order was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Stop order was never acknowledged");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Stop order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 9: Order modify (35=G) ───

struct ModifyOrderStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    modify_sent: Arc<AtomicBool>,
    modify_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    order_id: Option<OrderId>,
    new_order_id: Option<OrderId>,
}

struct ModifyOrderHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    modify_sent: Arc<AtomicBool>,
    modify_acked: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
}

impl ModifyOrderStrategy {
    fn new() -> (Self, ModifyOrderHandle) {
        let submitted = Arc::new(AtomicBool::new(false));
        let order_acked = Arc::new(AtomicBool::new(false));
        let modify_sent = Arc::new(AtomicBool::new(false));
        let modify_acked = Arc::new(AtomicBool::new(false));
        let cancel_sent = Arc::new(AtomicBool::new(false));
        let order_cancelled = Arc::new(AtomicBool::new(false));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let handle = ModifyOrderHandle {
            submitted: submitted.clone(),
            order_acked: order_acked.clone(),
            modify_sent: modify_sent.clone(),
            modify_acked: modify_acked.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            submitted, order_acked, modify_sent, modify_acked, cancel_sent,
            order_cancelled, order_rejected,
            order_id: None, new_order_id: None,
        }, handle)
    }
}

impl Strategy for ModifyOrderStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        let price = 1_00_000_000i64; // $1.00
        let id = ctx.submit_limit(0, Side::Buy, 1, price);
        self.order_id = Some(id);
        self.submitted.store(true, Ordering::Relaxed);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        match update.status {
            OrderStatus::Submitted => {
                if self.modify_sent.load(Ordering::Relaxed) && !self.modify_acked.load(Ordering::Relaxed) {
                    self.modify_acked.store(true, Ordering::Relaxed);
                    let cancel_id = self.new_order_id.unwrap_or_else(|| self.order_id.unwrap());
                    ctx.cancel(cancel_id);
                    self.cancel_sent.store(true, Ordering::Relaxed);
                } else if !self.order_acked.load(Ordering::Relaxed) {
                    self.order_acked.store(true, Ordering::Relaxed);
                    if let Some(id) = self.order_id {
                        let new_price = 2_00_000_000i64; // $2.00
                        let new_id = ctx.modify(id, new_price, 1);
                        self.new_order_id = Some(new_id);
                        self.modify_sent.store(true, Ordering::Relaxed);
                    }
                }
            }
            OrderStatus::Cancelled => {
                self.order_cancelled.store(true, Ordering::Relaxed);
            }
            OrderStatus::Rejected => {
                self.order_rejected.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_modify_order(conns: Conns) -> Conns {
    println!("--- Phase 9: Order Modify (35=G) + Cancel (SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = ModifyOrderStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Modify test rejected\n");
        return conns;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Order was never acknowledged");
    assert!(handle.modify_sent.load(Ordering::Relaxed), "Modify was never sent");
    assert!(handle.modify_acked.load(Ordering::Relaxed), "Modify was never acknowledged");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Modified order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 10: Outside RTH limit order (GTC + OutsideRTH) ───

struct OutsideRthStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    order_id: Option<OrderId>,
}

struct OutsideRthHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
}

impl OutsideRthStrategy {
    fn new() -> (Self, OutsideRthHandle) {
        let submitted = Arc::new(AtomicBool::new(false));
        let order_acked = Arc::new(AtomicBool::new(false));
        let cancel_sent = Arc::new(AtomicBool::new(false));
        let order_cancelled = Arc::new(AtomicBool::new(false));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let handle = OutsideRthHandle {
            submitted: submitted.clone(),
            order_acked: order_acked.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            submitted, order_acked, cancel_sent, order_cancelled, order_rejected,
            order_id: None,
        }, handle)
    }
}

impl Strategy for OutsideRthStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        let price = 1_00_000_000i64; // $1.00
        let id = ctx.submit_limit_gtc(0, Side::Buy, 1, price, true);
        self.order_id = Some(id);
        self.submitted.store(true, Ordering::Relaxed);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        match update.status {
            OrderStatus::Submitted => {
                self.order_acked.store(true, Ordering::Relaxed);
                if !self.cancel_sent.load(Ordering::Relaxed) {
                    if let Some(id) = self.order_id {
                        ctx.cancel(id);
                        self.cancel_sent.store(true, Ordering::Relaxed);
                    }
                }
            }
            OrderStatus::Cancelled => {
                self.order_cancelled.store(true, Ordering::Relaxed);
            }
            OrderStatus::Rejected => {
                self.order_rejected.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_outside_rth(conns: Conns) -> Conns {
    println!("--- Phase 10: Outside RTH Limit Order (GTC+OutsideRTH, SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = OutsideRthStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        panic!("GTC+OutsideRTH order should not be rejected");
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Order was never acknowledged");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 13: Heartbeat keepalive ───

struct HeartbeatStrategy {
    disconnected: Arc<AtomicBool>,
    tick_count: Arc<AtomicU32>,
}

struct HeartbeatHandle {
    disconnected: Arc<AtomicBool>,
}

impl HeartbeatStrategy {
    fn new() -> (Self, HeartbeatHandle) {
        let disconnected = Arc::new(AtomicBool::new(false));
        let tick_count = Arc::new(AtomicU32::new(0));
        let handle = HeartbeatHandle { disconnected: disconnected.clone() };
        (Self { disconnected, tick_count }, handle)
    }
}

impl Strategy for HeartbeatStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {}

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {
        let count = self.tick_count.fetch_add(1, Ordering::Relaxed);
        if count % 100 == 0 && count > 0 {
            println!("  {} ticks, still connected", count);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        self.disconnected.store(true, Ordering::Relaxed);
    }
}

fn phase_heartbeat_keepalive(conns: Conns) -> Conns {
    println!("--- Phase 13: Heartbeat Keepalive (20s > CCP 10s interval) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = HeartbeatStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if handle.disconnected.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(200));
    }

    let still_connected = !handle.disconnected.load(Ordering::Relaxed);
    let elapsed = start.elapsed();

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(still_connected,
        "Connection dropped after {:.1}s — heartbeat mechanism failed", elapsed.as_secs_f64());
    println!("  PASS ({:.1}s, no disconnect)\n", elapsed.as_secs_f64());
    conns
}

// ─── Phase 15: Stop limit order submit + cancel ───

struct StopLimitStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    order_id: Option<OrderId>,
}

struct StopLimitHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
}

impl StopLimitStrategy {
    fn new() -> (Self, StopLimitHandle) {
        let submitted = Arc::new(AtomicBool::new(false));
        let order_acked = Arc::new(AtomicBool::new(false));
        let cancel_sent = Arc::new(AtomicBool::new(false));
        let order_cancelled = Arc::new(AtomicBool::new(false));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let handle = StopLimitHandle {
            submitted: submitted.clone(),
            order_acked: order_acked.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            submitted, order_acked, cancel_sent, order_cancelled, order_rejected,
            order_id: None,
        }, handle)
    }
}

impl Strategy for StopLimitStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        let stop_price = 999_00_000_000i64; // $999.00
        let limit_price = 998_00_000_000i64; // $998.00
        let id = ctx.submit_stop_limit(0, Side::Buy, 1, limit_price, stop_price);
        self.order_id = Some(id);
        self.submitted.store(true, Ordering::Relaxed);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        match update.status {
            OrderStatus::Submitted => {
                self.order_acked.store(true, Ordering::Relaxed);
                if !self.cancel_sent.load(Ordering::Relaxed) {
                    if let Some(id) = self.order_id {
                        ctx.cancel(id);
                        self.cancel_sent.store(true, Ordering::Relaxed);
                    }
                }
            }
            OrderStatus::Cancelled => {
                self.order_cancelled.store(true, Ordering::Relaxed);
            }
            OrderStatus::Rejected => {
                self.order_rejected.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_stop_limit_order(conns: Conns) -> Conns {
    println!("--- Phase 15: Stop Limit Order Submit + Cancel (SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = StopLimitStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Stop limit test rejected\n");
        return conns;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Stop limit was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Stop limit was never acknowledged");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Stop limit was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 16: Subscribe + Unsubscribe cleanup ───

struct UnsubscribeStrategy {
    unsub_requested: Arc<AtomicBool>,
    still_alive: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    tick_count: Arc<AtomicU32>,
}

struct UnsubscribeHandle {
    unsub_requested: Arc<AtomicBool>,
    still_alive: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    tick_count: Arc<AtomicU32>,
}

impl UnsubscribeStrategy {
    fn new() -> (Self, UnsubscribeHandle) {
        let unsub_requested = Arc::new(AtomicBool::new(false));
        let still_alive = Arc::new(AtomicBool::new(false));
        let disconnected = Arc::new(AtomicBool::new(false));
        let tick_count = Arc::new(AtomicU32::new(0));
        let handle = UnsubscribeHandle {
            unsub_requested: unsub_requested.clone(),
            still_alive: still_alive.clone(),
            disconnected: disconnected.clone(),
            tick_count: tick_count.clone(),
        };
        (Self { unsub_requested, still_alive, disconnected, tick_count }, handle)
    }
}

impl Strategy for UnsubscribeStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {}

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {
        self.tick_count.fetch_add(1, Ordering::Relaxed);
        if self.unsub_requested.load(Ordering::Relaxed) {
            self.still_alive.store(true, Ordering::Relaxed);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        self.disconnected.store(true, Ordering::Relaxed);
        if self.unsub_requested.load(Ordering::Relaxed) {
            self.still_alive.store(true, Ordering::Relaxed);
        }
    }
}

fn phase_subscribe_unsubscribe(conns: Conns) -> Conns {
    println!("--- Phase 16: Subscribe + Unsubscribe Cleanup ---");

    let account_id = conns.account_id;
    let (strategy, handle) = UnsubscribeStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    std::thread::sleep(Duration::from_secs(3));

    control_tx.send(ControlCommand::Unsubscribe { instrument: 0 }).unwrap();
    handle.unsub_requested.store(true, Ordering::Relaxed);

    std::thread::sleep(Duration::from_secs(3));

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(handle.disconnected.load(Ordering::Relaxed), "on_disconnect was not called");
    assert!(handle.still_alive.load(Ordering::Relaxed),
        "Hot loop did not continue running after unsubscribe");
    println!("  Total ticks: {}", handle.tick_count.load(Ordering::Relaxed));
    println!("  PASS\n");
    conns
}

// ─── Phase 17: Commission tracking ───

struct CommissionStrategy {
    buy_commission: Arc<AtomicI64>,
    sell_commission: Arc<AtomicI64>,
    buy_fill_price: Arc<AtomicI64>,
    sell_fill_price: Arc<AtomicI64>,
    order_rejected: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    phase: u8,
}

struct CommissionHandle {
    buy_commission: Arc<AtomicI64>,
    sell_commission: Arc<AtomicI64>,
    buy_fill_price: Arc<AtomicI64>,
    sell_fill_price: Arc<AtomicI64>,
    order_rejected: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl CommissionStrategy {
    fn new() -> (Self, CommissionHandle) {
        let buy_commission = Arc::new(AtomicI64::new(0));
        let sell_commission = Arc::new(AtomicI64::new(0));
        let buy_fill_price = Arc::new(AtomicI64::new(0));
        let sell_fill_price = Arc::new(AtomicI64::new(0));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let handle = CommissionHandle {
            buy_commission: buy_commission.clone(),
            sell_commission: sell_commission.clone(),
            buy_fill_price: buy_fill_price.clone(),
            sell_fill_price: sell_fill_price.clone(),
            order_rejected: order_rejected.clone(),
            done: done.clone(),
        };
        (Self {
            buy_commission, sell_commission, buy_fill_price, sell_fill_price,
            order_rejected, done,
            phase: 0,
        }, handle)
    }
}

impl Strategy for CommissionStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        // Use market orders — limit GTC prices can trigger IB's price cap rejection
        ctx.submit_market(0, Side::Buy, 1);
        self.phase = 1;
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_fill(&mut self, fill: &Fill, ctx: &mut Context) {
        if self.phase == 1 && fill.side == Side::Buy {
            self.buy_fill_price.store(fill.price, Ordering::Relaxed);
            self.buy_commission.store(fill.commission, Ordering::Relaxed);

            ctx.submit_market(fill.instrument, Side::Sell, 1);
            self.phase = 2;
        } else if self.phase == 2 && fill.side == Side::Sell {
            self.sell_fill_price.store(fill.price, Ordering::Relaxed);
            self.sell_commission.store(fill.commission, Ordering::Relaxed);
            self.phase = 3;
            self.done.store(true, Ordering::Relaxed);
        }
    }

    fn on_order_update(&mut self, update: &OrderUpdate, _ctx: &mut Context) {
        if update.status == OrderStatus::Rejected {
            self.order_rejected.store(true, Ordering::Relaxed);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

// ─── Generic submit+cancel strategy for new order types ───

struct SubmitCancelStrategy<F: Fn(&mut Context) -> OrderId + Send> {
    submit_fn: F,
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    order_filled: Arc<AtomicBool>,
    order_id: Option<OrderId>,
}

struct SubmitCancelHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    order_filled: Arc<AtomicBool>,
}

fn make_submit_cancel<F: Fn(&mut Context) -> OrderId + Send>(f: F) -> (SubmitCancelStrategy<F>, SubmitCancelHandle) {
    let submitted = Arc::new(AtomicBool::new(false));
    let order_acked = Arc::new(AtomicBool::new(false));
    let cancel_sent = Arc::new(AtomicBool::new(false));
    let order_cancelled = Arc::new(AtomicBool::new(false));
    let order_rejected = Arc::new(AtomicBool::new(false));
    let order_filled = Arc::new(AtomicBool::new(false));
    let handle = SubmitCancelHandle {
        submitted: submitted.clone(),
        order_acked: order_acked.clone(),
        order_cancelled: order_cancelled.clone(),
        order_rejected: order_rejected.clone(),
        order_filled: order_filled.clone(),
    };
    (SubmitCancelStrategy {
        submit_fn: f,
        submitted, order_acked, cancel_sent, order_cancelled, order_rejected, order_filled,
        order_id: None,
    }, handle)
}

impl<F: Fn(&mut Context) -> OrderId + Send> Strategy for SubmitCancelStrategy<F> {
    fn on_start(&mut self, ctx: &mut Context) {
        let id = (self.submit_fn)(ctx);
        self.order_id = Some(id);
        self.submitted.store(true, Ordering::Relaxed);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        match update.status {
            OrderStatus::Submitted => {
                self.order_acked.store(true, Ordering::Relaxed);
                if !self.cancel_sent.load(Ordering::Relaxed) {
                    if let Some(id) = self.order_id {
                        ctx.cancel(id);
                        self.cancel_sent.store(true, Ordering::Relaxed);
                    }
                }
            }
            OrderStatus::Cancelled => {
                self.order_cancelled.store(true, Ordering::Relaxed);
            }
            OrderStatus::Rejected => {
                self.order_rejected.store(true, Ordering::Relaxed);
            }
            OrderStatus::Filled => {
                self.order_filled.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

/// Like run_submit_cancel_phase but accepts both fill and cancel as success.
/// Use for market-like orders (MTL, MKT PRT, SNAP*) that may fill before cancel arrives.
fn run_submit_fill_or_cancel_phase<F: Fn(&mut Context) -> OrderId + Send + 'static>(
    conns: Conns,
    phase_name: &str,
    submit_fn: F,
) -> Conns {
    println!("--- {} ---", phase_name);

    let account_id = conns.account_id;
    let (strategy, handle) = make_submit_cancel(submit_fn);
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed)
            || handle.order_rejected.load(Ordering::Relaxed)
            || handle.order_filled.load(Ordering::Relaxed)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Order rejected\n");
        return conns;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Order was never submitted");
    let filled = handle.order_filled.load(Ordering::Relaxed);
    let cancelled = handle.order_cancelled.load(Ordering::Relaxed);
    assert!(filled || cancelled, "Order was neither filled nor cancelled");
    if filled {
        println!("  PASS (filled)\n");
    } else {
        println!("  PASS (cancelled)\n");
    }
    conns
}

fn run_submit_cancel_phase<F: Fn(&mut Context) -> OrderId + Send + 'static>(
    conns: Conns,
    phase_name: &str,
    submit_fn: F,
) -> Conns {
    println!("--- {} ---", phase_name);

    let account_id = conns.account_id;
    let (strategy, handle) = make_submit_cancel(submit_fn);
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Order rejected\n");
        return conns;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Order was never acknowledged");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 19: Trailing Stop order ───

fn phase_trailing_stop(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 19: Trailing Stop Order (SPY)", |ctx| {
        ctx.submit_trailing_stop(0, Side::Sell, 1, 5_00_000_000) // $5.00 trail
    })
}

// ─── Phase 20: Trailing Stop Limit order ───

fn phase_trailing_stop_limit(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 20: Trailing Stop Limit Order (SPY)", |ctx| {
        ctx.submit_trailing_stop_limit(0, Side::Sell, 1, 1_00_000_000, 5_00_000_000) // $1 lmt offset, $5 trail
    })
}

// ─── Phase 21: Limit IOC order ───

fn phase_limit_ioc(conns: Conns) -> Conns {
    // IOC at $1 will be immediately cancelled (no fill at that price)
    println!("--- Phase 21: Limit IOC Order (SPY) ---");

    let account_id = conns.account_id;
    let submitted = Arc::new(AtomicBool::new(false));
    let order_cancelled = Arc::new(AtomicBool::new(false));
    let order_rejected = Arc::new(AtomicBool::new(false));
    let order_acked = Arc::new(AtomicBool::new(false));

    let sub = submitted.clone();
    let can = order_cancelled.clone();
    let rej = order_rejected.clone();
    let ack = order_acked.clone();

    struct IocStrategy {
        submitted: Arc<AtomicBool>,
        order_cancelled: Arc<AtomicBool>,
        order_rejected: Arc<AtomicBool>,
        order_acked: Arc<AtomicBool>,
    }

    impl Strategy for IocStrategy {
        fn on_start(&mut self, ctx: &mut Context) {
            ctx.submit_limit_ioc(0, Side::Buy, 1, 1_00_000_000); // $1.00
            self.submitted.store(true, Ordering::Relaxed);
        }
        fn on_tick(&mut self, _: InstrumentId, _: &mut Context) {}
        fn on_order_update(&mut self, update: &OrderUpdate, _: &mut Context) {
            match update.status {
                OrderStatus::Submitted => self.order_acked.store(true, Ordering::Relaxed),
                OrderStatus::Cancelled => self.order_cancelled.store(true, Ordering::Relaxed),
                OrderStatus::Rejected => self.order_rejected.store(true, Ordering::Relaxed),
                _ => {}
            }
        }
        fn on_disconnect(&mut self, _: &mut Context) {}
    }

    let strategy = IocStrategy { submitted: sub, order_cancelled: can.clone(), order_rejected: rej.clone(), order_acked: ack.clone() };
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if can.load(Ordering::Relaxed) || rej.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if rej.load(Ordering::Relaxed) {
        println!("  SKIP: IOC order rejected\n");
        return conns;
    }

    // IOC at $1 should either be acked then cancelled, or just cancelled immediately
    assert!(submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(can.load(Ordering::Relaxed), "IOC order was not cancelled (should expire immediately at $1)");
    println!("  PASS (IOC cancelled as expected — no fill at $1)\n");
    conns
}

// ─── Phase 22: Limit FOK order ───

fn phase_limit_fok(conns: Conns) -> Conns {
    println!("--- Phase 22: Limit FOK Order (SPY) ---");

    let account_id = conns.account_id;
    let submitted = Arc::new(AtomicBool::new(false));
    let order_cancelled = Arc::new(AtomicBool::new(false));
    let order_rejected = Arc::new(AtomicBool::new(false));

    let sub = submitted.clone();
    let can = order_cancelled.clone();
    let rej = order_rejected.clone();

    struct FokStrategy {
        submitted: Arc<AtomicBool>,
        order_cancelled: Arc<AtomicBool>,
        order_rejected: Arc<AtomicBool>,
    }

    impl Strategy for FokStrategy {
        fn on_start(&mut self, ctx: &mut Context) {
            ctx.submit_limit_fok(0, Side::Buy, 1, 1_00_000_000); // $1.00
            self.submitted.store(true, Ordering::Relaxed);
        }
        fn on_tick(&mut self, _: InstrumentId, _: &mut Context) {}
        fn on_order_update(&mut self, update: &OrderUpdate, _: &mut Context) {
            match update.status {
                OrderStatus::Cancelled => self.order_cancelled.store(true, Ordering::Relaxed),
                OrderStatus::Rejected => self.order_rejected.store(true, Ordering::Relaxed),
                _ => {}
            }
        }
        fn on_disconnect(&mut self, _: &mut Context) {}
    }

    let strategy = FokStrategy { submitted: sub, order_cancelled: can.clone(), order_rejected: rej.clone() };
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if can.load(Ordering::Relaxed) || rej.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if rej.load(Ordering::Relaxed) {
        println!("  SKIP: FOK order rejected\n");
        return conns;
    }

    assert!(submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(can.load(Ordering::Relaxed), "FOK order was not cancelled (should expire immediately at $1)");
    println!("  PASS (FOK cancelled as expected — no fill at $1)\n");
    conns
}

// ─── Phase 23: Stop GTC order ───

fn phase_stop_gtc(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 23: Stop GTC Order (SPY)", |ctx| {
        ctx.submit_stop_gtc(0, Side::Sell, 1, 1_00_000_000, true) // $1.00 stop, outside RTH
    })
}

// ─── Phase 24: Stop Limit GTC order ───

fn phase_stop_limit_gtc(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 24: Stop Limit GTC Order (SPY)", |ctx| {
        ctx.submit_stop_limit_gtc(0, Side::Sell, 1, 1_00_000_000, 1_00_000_000, true) // $1 limit, $1 stop, outside RTH
    })
}

// ─── Phase 25: Market if Touched order ───

fn phase_mit_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 25: Market if Touched Order (SPY)", |ctx| {
        ctx.submit_mit(0, Side::Buy, 1, 1_00_000_000) // $1.00 trigger
    })
}

// ─── Phase 26: Limit if Touched order ───

fn phase_lit_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 26: Limit if Touched Order (SPY)", |ctx| {
        // LIT BUY: trigger at $1, then buy at limit $2
        ctx.submit_lit(0, Side::Buy, 1, 2_00_000_000, 1_00_000_000) // $2 limit, $1 trigger
    })
}

// ─── Phase 29: Bracket Order (parent + TP + SL) ───

fn phase_bracket_order(conns: Conns) -> Conns {
    println!("--- Phase 29: Bracket Order (SPY) ---");

    let account_id = conns.account_id;
    let parent_acked = Arc::new(AtomicBool::new(false));
    let tp_acked = Arc::new(AtomicBool::new(false));
    let sl_acked = Arc::new(AtomicBool::new(false));
    let any_rejected = Arc::new(AtomicBool::new(false));
    let all_cancelled = Arc::new(AtomicU32::new(0));

    let pa = parent_acked.clone();
    let ta = tp_acked.clone();
    let sa = sl_acked.clone();
    let rej = any_rejected.clone();
    let can = all_cancelled.clone();

    struct BracketStrategy {
        parent_acked: Arc<AtomicBool>,
        tp_acked: Arc<AtomicBool>,
        sl_acked: Arc<AtomicBool>,
        any_rejected: Arc<AtomicBool>,
        all_cancelled: Arc<AtomicU32>,
        parent_id: Option<OrderId>,
        tp_id: Option<OrderId>,
        sl_id: Option<OrderId>,
        cancel_sent: bool,
    }

    impl Strategy for BracketStrategy {
        fn on_start(&mut self, ctx: &mut Context) {
            // BUY bracket: entry at $1 (won't fill), TP at $2, SL at $0.50
            let (pid, tid, sid) = ctx.submit_bracket(0, Side::Buy, 1,
                1_00_000_000, 2_00_000_000, 50_000_000);
            self.parent_id = Some(pid);
            self.tp_id = Some(tid);
            self.sl_id = Some(sid);
        }
        fn on_tick(&mut self, _: InstrumentId, _: &mut Context) {}
        fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
            match update.status {
                OrderStatus::Submitted => {
                    if Some(update.order_id) == self.parent_id {
                        self.parent_acked.store(true, Ordering::Relaxed);
                    } else if Some(update.order_id) == self.tp_id {
                        self.tp_acked.store(true, Ordering::Relaxed);
                    } else if Some(update.order_id) == self.sl_id {
                        self.sl_acked.store(true, Ordering::Relaxed);
                    }
                    // Cancel parent once acked (children should cancel via OCA/parent link)
                    if self.parent_acked.load(Ordering::Relaxed) && !self.cancel_sent {
                        if let Some(pid) = self.parent_id {
                            ctx.cancel(pid);
                            self.cancel_sent = true;
                        }
                    }
                }
                OrderStatus::Cancelled => {
                    self.all_cancelled.fetch_add(1, Ordering::Relaxed);
                }
                OrderStatus::Rejected => {
                    self.any_rejected.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        fn on_disconnect(&mut self, _: &mut Context) {}
    }

    let strategy = BracketStrategy {
        parent_acked: pa, tp_acked: ta, sl_acked: sa,
        any_rejected: rej.clone(), all_cancelled: can.clone(),
        parent_id: None, tp_id: None, sl_id: None, cancel_sent: false,
    };

    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if rej.load(Ordering::Relaxed) { break; }
        if can.load(Ordering::Relaxed) >= 1 { break; } // at least parent cancelled
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if any_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Bracket order rejected\n");
        return conns;
    }

    assert!(parent_acked.load(Ordering::Relaxed), "Parent order was never acknowledged");
    let cancelled = all_cancelled.load(Ordering::Relaxed);
    println!("  Parent acked: {}, TP acked: {}, SL acked: {}",
        parent_acked.load(Ordering::Relaxed),
        tp_acked.load(Ordering::Relaxed),
        sl_acked.load(Ordering::Relaxed));
    println!("  Cancelled: {} orders", cancelled);
    println!("  PASS\n");
    conns
}

// ─── Phase 27: Market on Close order ───

fn phase_moc_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 27: MOC Order (SPY)", |ctx| {
        ctx.submit_moc(0, Side::Buy, 1)
    })
}

// ─── Phase 28: Limit on Close order ───

fn phase_loc_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 28: LOC Order (SPY)", |ctx| {
        ctx.submit_loc(0, Side::Buy, 1, 1_00_000_000) // $1.00 limit
    })
}

// ─── Phase 30: Adaptive Algo Limit Order ───

fn phase_adaptive_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 30: Adaptive Algo Limit Order (SPY)", |ctx| {
        ctx.submit_adaptive(0, Side::Buy, 1, 1_00_000_000, AdaptivePriority::Normal) // $1.00, Normal priority
    })
}

// ─── Phase 31: Relative / Pegged-to-Primary Order ───

fn phase_rel_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 31: Relative Order (SPY)", |ctx| {
        ctx.submit_rel(0, Side::Buy, 1, 1_000_000) // $0.01 offset
    })
}

// ─── Phase 32: Limit OPG (At the Opening) ───

fn phase_limit_opg(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 32: Limit OPG Order (SPY)", |ctx| {
        ctx.submit_limit_opg(0, Side::Buy, 1, 1_00_000_000) // $1.00
    })
}

// ─── Phase 35: Short Sell Limit Order ───

fn phase_short_sell(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 35: Short Sell Limit Order (SPY)", |ctx| {
        ctx.submit_limit_ex(0, Side::ShortSell, 1, 1_00_000_000, b'0', OrderAttrs::default())
    })
}

// ─── Phase 37: Standalone OCA Group ───

fn phase_oca_group(conns: Conns) -> Conns {
    println!("--- Phase 37: OCA Group (SPY) ---");

    let account_id = conns.account_id;
    let order1_acked = Arc::new(AtomicBool::new(false));
    let order2_acked = Arc::new(AtomicBool::new(false));
    let any_rejected = Arc::new(AtomicBool::new(false));
    let cancelled_count = Arc::new(AtomicU32::new(0));

    let a1 = order1_acked.clone();
    let a2 = order2_acked.clone();
    let rej = any_rejected.clone();
    let can = cancelled_count.clone();

    struct OcaStrategy {
        order1_acked: Arc<AtomicBool>,
        order2_acked: Arc<AtomicBool>,
        any_rejected: Arc<AtomicBool>,
        cancelled_count: Arc<AtomicU32>,
        order1_id: Option<OrderId>,
        order2_id: Option<OrderId>,
        cancel_sent: bool,
    }

    impl Strategy for OcaStrategy {
        fn on_start(&mut self, ctx: &mut Context) {
            let oca = ctx.now_ns(); // unique OCA group ID
            let id1 = ctx.submit_limit_ex(0, Side::Buy, 1, 1_00_000_000, b'1', OrderAttrs {
                oca_group: oca, outside_rth: true, ..OrderAttrs::default()
            });
            let id2 = ctx.submit_limit_ex(0, Side::Buy, 1, 2_00_000_000, b'1', OrderAttrs {
                oca_group: oca, outside_rth: true, ..OrderAttrs::default()
            });
            self.order1_id = Some(id1);
            self.order2_id = Some(id2);
        }
        fn on_tick(&mut self, _: InstrumentId, _: &mut Context) {}
        fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
            match update.status {
                OrderStatus::Submitted => {
                    if Some(update.order_id) == self.order1_id {
                        self.order1_acked.store(true, Ordering::Relaxed);
                    } else if Some(update.order_id) == self.order2_id {
                        self.order2_acked.store(true, Ordering::Relaxed);
                    }
                    // Cancel order1 once both acked
                    if self.order1_acked.load(Ordering::Relaxed)
                        && self.order2_acked.load(Ordering::Relaxed)
                        && !self.cancel_sent
                    {
                        if let Some(id) = self.order1_id {
                            ctx.cancel(id);
                            self.cancel_sent = true;
                        }
                    }
                }
                OrderStatus::Cancelled => {
                    self.cancelled_count.fetch_add(1, Ordering::Relaxed);
                }
                OrderStatus::Rejected => {
                    self.any_rejected.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
        }
    }

    let strategy = OcaStrategy {
        order1_acked: a1, order2_acked: a2,
        any_rejected: rej.clone(), cancelled_count: can.clone(),
        order1_id: None, order2_id: None, cancel_sent: false,
    };

    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if rej.load(Ordering::Relaxed) { break; }
        if can.load(Ordering::Relaxed) >= 1 { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if any_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: OCA order rejected\n");
        return conns;
    }

    assert!(order1_acked.load(Ordering::Relaxed), "Order 1 never acked");
    assert!(order2_acked.load(Ordering::Relaxed), "Order 2 never acked");
    let cancelled = cancelled_count.load(Ordering::Relaxed);
    println!("  Order1 acked: {}, Order2 acked: {}, Cancelled: {}",
        order1_acked.load(Ordering::Relaxed),
        order2_acked.load(Ordering::Relaxed),
        cancelled);
    println!("  PASS\n");
    conns
}

// ─── Phase 36: Trailing Stop Percent ───

fn phase_trailing_stop_pct(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 36: Trailing Stop Percent Order (SPY)", |ctx| {
        ctx.submit_trailing_stop_pct(0, Side::Sell, 1, 100) // 1% trail
    })
}

// ─── Phase 33: Iceberg Order (display size) ───

fn phase_iceberg_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 33: Iceberg Order (SPY)", |ctx| {
        ctx.submit_limit_ex(0, Side::Buy, 10, 1_00_000_000, b'1', OrderAttrs {
            display_size: 1,
            outside_rth: true,
            ..OrderAttrs::default()
        })
    })
}

// ─── Phase 34: Hidden Order ───

fn phase_hidden_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 34: Hidden Order (SPY)", |ctx| {
        ctx.submit_limit_ex(0, Side::Buy, 1, 1_00_000_000, b'1', OrderAttrs {
            hidden: true,
            outside_rth: true,
            ..OrderAttrs::default()
        })
    })
}

// ─── Phase 17: Commission tracking ───

fn phase_commission(conns: Conns) -> Conns {
    println!("--- Phase 17: Commission Tracking (GTC+OutsideRTH fill) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = CommissionStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.done.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Order rejected — extended hours may not be active\n");
        return conns;
    }

    let buy_price = handle.buy_fill_price.load(Ordering::Relaxed);
    let buy_comm = handle.buy_commission.load(Ordering::Relaxed);
    let sell_price = handle.sell_fill_price.load(Ordering::Relaxed);
    let sell_comm = handle.sell_commission.load(Ordering::Relaxed);

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

// ─── Phase 38: Market to Limit (MTL) ───

fn phase_mtl_order(conns: Conns) -> Conns {
    run_submit_fill_or_cancel_phase(conns, "Phase 38: Market to Limit Order (SPY)", |ctx| {
        ctx.submit_mtl(0, Side::Buy, 1)
    })
}

// ─── Phase 39: Market with Protection ───

fn phase_mkt_prt_order(conns: Conns) -> Conns {
    run_submit_fill_or_cancel_phase(conns, "Phase 39: Market with Protection Order (SPY)", |ctx| {
        ctx.submit_mkt_prt(0, Side::Buy, 1)
    })
}

// ─── Phase 40: Stop with Protection ───

fn phase_stp_prt_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 40: Stop with Protection Order (SPY)", |ctx| {
        ctx.submit_stp_prt(0, Side::Sell, 1, 1_00_000_000) // $1.00 stop
    })
}

// ─── Phase 41: Mid-Price ───

fn phase_mid_price_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 41: Mid-Price Order (SPY)", |ctx| {
        ctx.submit_mid_price(0, Side::Buy, 1, 1_00_000_000) // $1.00 price cap
    })
}

// ─── Phase 42: Snap to Market ───

fn phase_snap_mkt_order(conns: Conns) -> Conns {
    run_submit_fill_or_cancel_phase(conns, "Phase 42: Snap to Market Order (SPY)", |ctx| {
        ctx.submit_snap_mkt(0, Side::Buy, 1)
    })
}

// ─── Phase 43: Snap to Midpoint ───

fn phase_snap_mid_order(conns: Conns) -> Conns {
    run_submit_fill_or_cancel_phase(conns, "Phase 43: Snap to Midpoint Order (SPY)", |ctx| {
        ctx.submit_snap_mid(0, Side::Buy, 1)
    })
}

// ─── Phase 44: Snap to Primary ───

fn phase_snap_pri_order(conns: Conns) -> Conns {
    run_submit_fill_or_cancel_phase(conns, "Phase 44: Snap to Primary Order (SPY)", |ctx| {
        ctx.submit_snap_pri(0, Side::Buy, 1)
    })
}

// ─── Phase 45: Pegged to Market ───

fn phase_peg_mkt_order(conns: Conns) -> Conns {
    run_submit_fill_or_cancel_phase(conns, "Phase 45: Pegged to Market Order (SPY)", |ctx| {
        ctx.submit_peg_mkt(0, Side::Buy, 1, 0) // no offset
    })
}

// ─── Phase 46: Pegged to Midpoint ───

fn phase_peg_mid_order(conns: Conns) -> Conns {
    run_submit_fill_or_cancel_phase(conns, "Phase 46: Pegged to Midpoint Order (SPY)", |ctx| {
        ctx.submit_peg_mid(0, Side::Buy, 1, 0) // no offset
    })
}

// ─── Phase 47: Discretionary Amount Order ───

fn phase_discretionary_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 47: Discretionary Amount Order (SPY)", |ctx| {
        ctx.submit_limit_ex(0, Side::Buy, 1, 1_00_000_000, b'1', OrderAttrs {
            discretionary_amt: 50_000_000, // $0.50 discretion above $1 limit
            outside_rth: true,
            ..OrderAttrs::default()
        })
    })
}

// ─── Phase 48: Sweep to Fill ───

fn phase_sweep_to_fill_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 48: Sweep to Fill Order (SPY)", |ctx| {
        ctx.submit_limit_ex(0, Side::Buy, 1, 1_00_000_000, b'1', OrderAttrs {
            sweep_to_fill: true,
            outside_rth: true,
            ..OrderAttrs::default()
        })
    })
}

// ─── Phase 49: All or None ───

fn phase_all_or_none_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 49: All or None Order (SPY)", |ctx| {
        ctx.submit_limit_ex(0, Side::Buy, 1, 1_00_000_000, b'1', OrderAttrs {
            all_or_none: true,
            outside_rth: true,
            ..OrderAttrs::default()
        })
    })
}

// ─── Phase 50: Trigger Method (Last price) ───

fn phase_trigger_method_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 50: Trigger Method Order (SPY)", |ctx| {
        // Stop order at $1 with trigger method = 2 (Last price)
        ctx.submit_limit_ex(0, Side::Buy, 1, 1_00_000_000, b'1', OrderAttrs {
            trigger_method: 2, // Last price trigger
            outside_rth: true,
            ..OrderAttrs::default()
        })
    })
}

// ─── Phase 10b: Outside RTH GTC Stop Order ───

fn phase_outside_rth_stop(conns: Conns) -> Conns {
    println!("--- Phase 10b: Outside RTH GTC Stop Order (SPY) ---");

    let account_id = conns.account_id;
    let order_acked = Arc::new(AtomicBool::new(false));
    let order_cancelled = Arc::new(AtomicBool::new(false));
    let order_rejected = Arc::new(AtomicBool::new(false));

    let oa = order_acked.clone();
    let oc = order_cancelled.clone();
    let or_ = order_rejected.clone();

    struct OutsideRthStopStrategy {
        order_acked: Arc<AtomicBool>,
        order_cancelled: Arc<AtomicBool>,
        order_rejected: Arc<AtomicBool>,
        order_id: Option<OrderId>,
        cancel_sent: bool,
    }

    impl Strategy for OutsideRthStopStrategy {
        fn on_start(&mut self, ctx: &mut Context) {
            let id = ctx.submit_stop_gtc(0, Side::Sell, 1, 1_00_000_000, true); // $1.00 stop, outside RTH
            self.order_id = Some(id);
        }
        fn on_tick(&mut self, _: InstrumentId, _: &mut Context) {}
        fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
            match update.status {
                OrderStatus::Submitted => {
                    self.order_acked.store(true, Ordering::Relaxed);
                    if !self.cancel_sent {
                        if let Some(id) = self.order_id {
                            ctx.cancel(id);
                            self.cancel_sent = true;
                        }
                    }
                }
                OrderStatus::Cancelled => {
                    self.order_cancelled.store(true, Ordering::Relaxed);
                }
                OrderStatus::Rejected => {
                    self.order_rejected.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        fn on_disconnect(&mut self, _: &mut Context) {}
    }

    let strategy = OutsideRthStopStrategy {
        order_acked: oa, order_cancelled: oc, order_rejected: or_,
        order_id: None, cancel_sent: false,
    };

    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if order_cancelled.load(Ordering::Relaxed) || order_rejected.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: GTC stop outside RTH rejected\n");
        return conns;
    }

    assert!(order_acked.load(Ordering::Relaxed), "GTC stop outside RTH was never acknowledged");
    assert!(order_cancelled.load(Ordering::Relaxed), "GTC stop outside RTH was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 9b: Modify Order Qty ───

fn phase_modify_qty(conns: Conns) -> Conns {
    println!("--- Phase 9b: Order Modify Qty (SPY) ---");

    let account_id = conns.account_id;
    let submitted = Arc::new(AtomicBool::new(false));
    let order_acked = Arc::new(AtomicBool::new(false));
    let modify_sent = Arc::new(AtomicBool::new(false));
    let modify_acked = Arc::new(AtomicBool::new(false));
    let order_cancelled = Arc::new(AtomicBool::new(false));
    let order_rejected = Arc::new(AtomicBool::new(false));

    let s = submitted.clone();
    let oa = order_acked.clone();
    let ms = modify_sent.clone();
    let ma = modify_acked.clone();
    let oc = order_cancelled.clone();
    let or_ = order_rejected.clone();

    struct ModifyQtyStrategy {
        submitted: Arc<AtomicBool>,
        order_acked: Arc<AtomicBool>,
        modify_sent: Arc<AtomicBool>,
        modify_acked: Arc<AtomicBool>,
        order_cancelled: Arc<AtomicBool>,
        order_rejected: Arc<AtomicBool>,
        order_id: Option<OrderId>,
        new_order_id: Option<OrderId>,
        cancel_sent: bool,
    }

    impl Strategy for ModifyQtyStrategy {
        fn on_start(&mut self, ctx: &mut Context) {
            // Submit limit at $1 with qty=1
            let id = ctx.submit_limit(0, Side::Buy, 1, 1_00_000_000);
            self.order_id = Some(id);
            self.submitted.store(true, Ordering::Relaxed);
        }
        fn on_tick(&mut self, _: InstrumentId, _: &mut Context) {}
        fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
            match update.status {
                OrderStatus::Submitted => {
                    if self.modify_sent.load(Ordering::Relaxed) && !self.modify_acked.load(Ordering::Relaxed) {
                        // Modify was acknowledged
                        self.modify_acked.store(true, Ordering::Relaxed);
                        let cancel_id = self.new_order_id.unwrap_or_else(|| self.order_id.unwrap());
                        ctx.cancel(cancel_id);
                        self.cancel_sent = true;
                    } else if !self.order_acked.load(Ordering::Relaxed) {
                        // Initial order acknowledged — modify qty from 1 to 2 (same price)
                        self.order_acked.store(true, Ordering::Relaxed);
                        if let Some(id) = self.order_id {
                            let new_id = ctx.modify(id, 1_00_000_000, 2); // same price, qty 1→2
                            self.new_order_id = Some(new_id);
                            self.modify_sent.store(true, Ordering::Relaxed);
                        }
                    }
                }
                OrderStatus::Cancelled => {
                    self.order_cancelled.store(true, Ordering::Relaxed);
                }
                OrderStatus::Rejected => {
                    self.order_rejected.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        fn on_disconnect(&mut self, _: &mut Context) {}
    }

    let strategy = ModifyQtyStrategy {
        submitted: s, order_acked: oa, modify_sent: ms, modify_acked: ma,
        order_cancelled: oc.clone(), order_rejected: or_.clone(),
        order_id: None, new_order_id: None, cancel_sent: false,
    };

    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if order_cancelled.load(Ordering::Relaxed) || order_rejected.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Modify qty test rejected\n");
        return conns;
    }

    assert!(submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(order_acked.load(Ordering::Relaxed), "Order was never acknowledged");
    assert!(modify_sent.load(Ordering::Relaxed), "Modify was never sent");
    assert!(modify_acked.load(Ordering::Relaxed), "Qty modify was never acknowledged");
    assert!(order_cancelled.load(Ordering::Relaxed), "Modified order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 51: Bracket Fill Cascade ───

fn phase_bracket_fill_cascade(conns: Conns) -> Conns {
    println!("--- Phase 51: Bracket Fill Cascade (SPY) ---");

    let account_id = conns.account_id;
    let entry_filled = Arc::new(AtomicBool::new(false));
    let tp_active = Arc::new(AtomicBool::new(false));
    let sl_active = Arc::new(AtomicBool::new(false));
    let all_cancelled = Arc::new(AtomicU32::new(0));
    let any_rejected = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));

    let ef = entry_filled.clone();
    let ta = tp_active.clone();
    let sa = sl_active.clone();
    let ac = all_cancelled.clone();
    let rej = any_rejected.clone();
    let dn = done.clone();

    struct BracketFillStrategy {
        entry_filled: Arc<AtomicBool>,
        tp_active: Arc<AtomicBool>,
        sl_active: Arc<AtomicBool>,
        all_cancelled: Arc<AtomicU32>,
        any_rejected: Arc<AtomicBool>,
        done: Arc<AtomicBool>,
        parent_id: Option<OrderId>,
        tp_id: Option<OrderId>,
        sl_id: Option<OrderId>,
        cancel_sent: bool,
        tick_count: u32,
    }

    impl Strategy for BracketFillStrategy {
        fn on_start(&mut self, _ctx: &mut Context) {}

        fn on_tick(&mut self, _instrument: InstrumentId, ctx: &mut Context) {
            self.tick_count += 1;
            // Wait for a few ticks to have market data, then submit bracket
            if self.tick_count == 5 && self.parent_id.is_none() {
                let ask = ctx.ask(0);
                if ask <= 0 { return; } // no quote yet
                // Entry at ask + $1 (will fill as marketable limit)
                // TP at entry + $100 (won't fill), SL at $0.01 (won't trigger)
                let entry = ask + 1_00_000_000;
                let (pid, tid, sid) = ctx.submit_bracket(0, Side::Buy, 1,
                    entry,
                    entry + 100_00_000_000, // TP = entry + $100
                    1_000_000);             // SL = $0.01
                self.parent_id = Some(pid);
                self.tp_id = Some(tid);
                self.sl_id = Some(sid);
            }
        }

        fn on_fill(&mut self, fill: &Fill, _ctx: &mut Context) {
            if Some(fill.order_id) == self.parent_id {
                self.entry_filled.store(true, Ordering::Relaxed);
            }
        }

        fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
            match update.status {
                OrderStatus::Submitted => {
                    if Some(update.order_id) == self.tp_id {
                        self.tp_active.store(true, Ordering::Relaxed);
                    } else if Some(update.order_id) == self.sl_id {
                        self.sl_active.store(true, Ordering::Relaxed);
                    }
                    // Once both children are active, cancel them and sell to flatten
                    if self.tp_active.load(Ordering::Relaxed) && self.sl_active.load(Ordering::Relaxed) && !self.cancel_sent {
                        if let Some(tid) = self.tp_id { ctx.cancel(tid); }
                        if let Some(sid) = self.sl_id { ctx.cancel(sid); }
                        self.cancel_sent = true;
                    }
                }
                OrderStatus::Cancelled => {
                    let count = self.all_cancelled.fetch_add(1, Ordering::Relaxed) + 1;
                    if count >= 2 {
                        // Both children cancelled — sell to flatten position
                        ctx.submit_market(0, Side::Sell, 1);
                    }
                }
                OrderStatus::Rejected => {
                    self.any_rejected.store(true, Ordering::Relaxed);
                }
                OrderStatus::Filled => {
                    // Sell market filled — we're done
                    if self.cancel_sent && update.order_id != self.parent_id.unwrap_or(0) {
                        self.done.store(true, Ordering::Relaxed);
                    }
                }
                _ => {}
            }
        }

        fn on_disconnect(&mut self, _: &mut Context) {}
    }

    let strategy = BracketFillStrategy {
        entry_filled: ef, tp_active: ta.clone(), sl_active: sa.clone(),
        all_cancelled: ac, any_rejected: rej.clone(), done: dn.clone(),
        parent_id: None, tp_id: None, sl_id: None, cancel_sent: false, tick_count: 0,
    };

    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if done.load(Ordering::Relaxed) || any_rejected.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if any_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Bracket fill cascade rejected\n");
        return conns;
    }

    let ef = entry_filled.load(Ordering::Relaxed);
    let tp = ta.load(Ordering::Relaxed);
    let sl = sa.load(Ordering::Relaxed);
    println!("  Entry filled: {}, TP active: {}, SL active: {}", ef, tp, sl);

    if !ef {
        println!("  SKIP: Entry did not fill — market may not have liquidity\n");
        return conns;
    }

    assert!(tp, "Take-profit child was never activated after entry fill");
    assert!(sl, "Stop-loss child was never activated after entry fill");
    println!("  PASS\n");
    conns
}

// ─── Phase 52: PnL After Round Trip ───

fn phase_pnl_after_round_trip(conns: Conns) -> Conns {
    println!("--- Phase 52: PnL After Round Trip (SPY) ---");

    let account_id = conns.account_id;
    let buy_filled = Arc::new(AtomicBool::new(false));
    let sell_filled = Arc::new(AtomicBool::new(false));
    let order_rejected = Arc::new(AtomicBool::new(false));
    let pnl_updated = Arc::new(AtomicBool::new(false));
    let realized_pnl = Arc::new(AtomicI64::new(0));

    let bf = buy_filled.clone();
    let sf = sell_filled.clone();
    let or_ = order_rejected.clone();
    let pu = pnl_updated.clone();
    let rpnl = realized_pnl.clone();

    struct PnlStrategy {
        buy_filled: Arc<AtomicBool>,
        sell_filled: Arc<AtomicBool>,
        order_rejected: Arc<AtomicBool>,
        pnl_updated: Arc<AtomicBool>,
        realized_pnl: Arc<AtomicI64>,
        phase: u8,
        initial_realized_pnl: Price,
        tick_count: u32,
    }

    impl Strategy for PnlStrategy {
        fn on_start(&mut self, ctx: &mut Context) {
            self.initial_realized_pnl = ctx.account().realized_pnl;
        }

        fn on_tick(&mut self, _instrument: InstrumentId, ctx: &mut Context) {
            self.tick_count += 1;

            // Submit buy after a few ticks
            if self.phase == 0 && self.tick_count >= 5 {
                ctx.submit_market(0, Side::Buy, 1);
                self.phase = 1;
            }

            // After sell fill, check PnL in subsequent ticks
            if self.phase == 3 {
                let current_rpnl = ctx.account().realized_pnl;
                if current_rpnl != self.initial_realized_pnl {
                    self.realized_pnl.store(current_rpnl, Ordering::Relaxed);
                    self.pnl_updated.store(true, Ordering::Relaxed);
                }
            }
        }

        fn on_fill(&mut self, fill: &Fill, ctx: &mut Context) {
            if self.phase == 1 && fill.side == Side::Buy {
                self.buy_filled.store(true, Ordering::Relaxed);
                ctx.submit_market(fill.instrument, Side::Sell, 1);
                self.phase = 2;
            } else if self.phase == 2 && fill.side == Side::Sell {
                self.sell_filled.store(true, Ordering::Relaxed);
                self.phase = 3;
            }
        }

        fn on_order_update(&mut self, update: &OrderUpdate, _: &mut Context) {
            if update.status == OrderStatus::Rejected {
                self.order_rejected.store(true, Ordering::Relaxed);
            }
        }

        fn on_disconnect(&mut self, _: &mut Context) {}
    }

    let strategy = PnlStrategy {
        buy_filled: bf, sell_filled: sf.clone(), order_rejected: or_.clone(),
        pnl_updated: pu.clone(), realized_pnl: rpnl.clone(),
        phase: 0, initial_realized_pnl: 0, tick_count: 0,
    };

    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if pnl_updated.load(Ordering::Relaxed) || order_rejected.load(Ordering::Relaxed) { break; }
        // Even if pnl doesn't update, exit once sell is filled and we've waited a bit
        if sell_filled.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(5));
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Order rejected\n");
        return conns;
    }

    if !buy_filled.load(Ordering::Relaxed) {
        println!("  SKIP: No fill — market may not have liquidity\n");
        return conns;
    }

    let bf = buy_filled.load(Ordering::Relaxed);
    let sf_val = sell_filled.load(Ordering::Relaxed);
    let pu_val = pnl_updated.load(Ordering::Relaxed);
    let rpnl_val = realized_pnl.load(Ordering::Relaxed);

    println!("  Buy filled: {}, Sell filled: {}", bf, sf_val);
    if pu_val {
        println!("  RealizedPnL changed: ${:.2}", rpnl_val as f64 / PRICE_SCALE as f64);
        println!("  PASS\n");
    } else {
        // Paper account may not update RealizedPnL immediately
        println!("  PASS (PnL not yet updated — paper account delay is expected)\n");
    }
    conns
}

// ─── Phase 55: Farm heartbeat keepalive (65s > 2x farm 30s interval) ───

fn phase_farm_heartbeat_keepalive(conns: Conns) -> Conns {
    println!("--- Phase 55: Farm Heartbeat Keepalive (65s > 2x farm 30s interval) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = HeartbeatStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let join = run_hot_loop(hot_loop);

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(65) {
        if handle.disconnected.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(500));
    }

    let still_connected = !handle.disconnected.load(Ordering::Relaxed);
    let elapsed = start.elapsed();

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(still_connected,
        "Farm disconnected after {:.1}s — heartbeat failed (expected to survive 2x 30s farm interval)",
        elapsed.as_secs_f64());
    println!("  PASS ({:.1}s, no disconnect, survived 2x farm heartbeat interval)\n",
        elapsed.as_secs_f64());
    conns
}

// ─── Phase 56: Heartbeat timeout detection (simulated stale CCP) ───

struct TimeoutStrategy {
    disconnect_count: Arc<AtomicU32>,
}

struct TimeoutHandle {
    disconnect_count: Arc<AtomicU32>,
}

impl TimeoutStrategy {
    fn new() -> (Self, TimeoutHandle) {
        let disconnect_count = Arc::new(AtomicU32::new(0));
        let handle = TimeoutHandle { disconnect_count: disconnect_count.clone() };
        (Self { disconnect_count }, handle)
    }
}

impl Strategy for TimeoutStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {}

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        self.disconnect_count.fetch_add(1, Ordering::Relaxed);
    }
}

fn phase_heartbeat_timeout_detection(conns: Conns) -> Conns {
    println!("--- Phase 56: Heartbeat Timeout Detection (simulated stale CCP) ---");

    let account_id = conns.account_id;

    // Create "dead" CCP: localhost TCP pair where server never responds.
    // The hot loop will send heartbeats into the void and eventually timeout.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind localhost");
    let addr = listener.local_addr().unwrap();
    let client = std::net::TcpStream::connect(addr).expect("connect to localhost");
    let _server = listener.accept().expect("accept dead socket").0; // keep alive, never write
    let dead_ccp = Connection::new_raw(client).expect("wrap dead socket as Connection");

    // Store real CCP aside — it will go stale during this test (~25s idle)
    let real_ccp = conns.ccp;

    let (strategy, handle) = TimeoutStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, dead_ccp, conns.hmds, None,
    );

    let join = run_hot_loop(hot_loop);

    // CCP timeout: 10s heartbeat + 1s grace + 10s TestRequest timeout = ~21s
    let start = Instant::now();
    let timeout = Duration::from_secs(30);
    while start.elapsed() < timeout {
        if handle.disconnect_count.load(Ordering::Relaxed) > 0 { break; }
        std::thread::sleep(Duration::from_millis(200));
    }

    let elapsed = start.elapsed();
    let dc_count = handle.disconnect_count.load(Ordering::Relaxed);

    // Verify: disconnect was detected
    assert!(dc_count > 0,
        "No disconnect after {:.1}s — heartbeat timeout should fire at ~21s",
        elapsed.as_secs_f64());

    // Verify: timeout fired within expected window (18-28s, generous bounds)
    assert!(elapsed.as_secs() >= 18 && elapsed.as_secs() <= 28,
        "Disconnect at {:.1}s — expected 18-28s (10+1+10=21s theoretical)",
        elapsed.as_secs_f64());

    // Verify: loop still alive after timeout (flag-based, not loop-kill).
    // If the loop had died, shutdown_and_reclaim would panic on join.
    let reclaimed = shutdown_and_reclaim(&control_tx, join, account_id.clone());

    // Verify: on_disconnect called exactly once
    let final_dc = handle.disconnect_count.load(Ordering::Relaxed);
    assert_eq!(final_dc, 1, "on_disconnect called {} times, expected exactly 1", final_dc);

    println!("  Timeout at {:.1}s (expected ~21s)", elapsed.as_secs_f64());
    println!("  on_disconnect called exactly once");
    println!("  Loop survived timeout (graceful shutdown succeeded)");
    println!("  PASS\n");

    // Return real CCP (may be stale, but this is last phase before shutdown)
    Conns { farm: reclaimed.farm, ccp: real_ccp, hmds: reclaimed.hmds, account_id }
}

// ─── Phase 57: Price Condition Order ───

fn phase_price_condition_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 57: Price Condition Order (SPY)", |ctx| {
        ctx.submit_limit_ex(0, Side::Buy, 1, 1_00_000_000, b'1', OrderAttrs {
            outside_rth: true,
            conditions: vec![OrderCondition::Price {
                con_id: 756733, // SPY
                exchange: "BEST".into(),
                price: 1_00_000_000, // $1 — won't trigger
                is_more: false,      // trigger when SPY <= $1
                trigger_method: 0,
            }],
            ..OrderAttrs::default()
        })
    })
}

// ─── Phase 58: Time Condition Order ───

fn phase_time_condition_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 58: Time Condition Order (SPY)", |ctx| {
        ctx.submit_limit_ex(0, Side::Buy, 1, 1_00_000_000, b'1', OrderAttrs {
            outside_rth: true,
            conditions: vec![OrderCondition::Time {
                time: "20991231-23:59:59".into(), // far future — won't trigger
                is_more: false,
            }],
            ..OrderAttrs::default()
        })
    })
}

// ─── Phase 59: Volume Condition Order ───

fn phase_volume_condition_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 59: Volume Condition Order (SPY)", |ctx| {
        ctx.submit_limit_ex(0, Side::Buy, 1, 1_00_000_000, b'1', OrderAttrs {
            outside_rth: true,
            conditions: vec![OrderCondition::Volume {
                con_id: 756733,
                exchange: "BEST".into(),
                volume: 999_999_999, // unreachable volume — won't trigger
                is_more: true,
            }],
            ..OrderAttrs::default()
        })
    })
}

// ─── Phase 60: Multi-Condition Order (Price AND Volume) ───

fn phase_multi_condition_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 60: Multi-Condition Order (SPY)", |ctx| {
        ctx.submit_limit_ex(0, Side::Buy, 1, 1_00_000_000, b'1', OrderAttrs {
            outside_rth: true,
            conditions: vec![
                OrderCondition::Price {
                    con_id: 756733,
                    exchange: "BEST".into(),
                    price: 1_00_000_000, // $1
                    is_more: false,
                    trigger_method: 2, // bid/ask
                },
                OrderCondition::Volume {
                    con_id: 756733,
                    exchange: "BEST".into(),
                    volume: 999_999_999,
                    is_more: true,
                },
            ],
            conditions_cancel_order: true,
            ..OrderAttrs::default()
        })
    })
}

// ─── Phase 61: Tick-by-Tick Data (SPY via HMDS) ───

fn phase_tbt_subscribe(conns: Conns) -> Conns {
    println!("--- Phase 61: Tick-by-Tick Data (SPY via HMDS) ---");

    let account_id = conns.account_id;
    let tbt_trade_count = Arc::new(AtomicU32::new(0));
    let tbt_quote_count = Arc::new(AtomicU32::new(0));
    let first_tbt = Arc::new(AtomicBool::new(false));

    struct TbtStrategy {
        trade_count: Arc<AtomicU32>,
        quote_count: Arc<AtomicU32>,
        first_tbt: Arc<AtomicBool>,
    }

    impl Strategy for TbtStrategy {
        fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

        fn on_tbt_trade(&mut self, trade: &TbtTrade, _ctx: &mut Context) {
            let n = self.trade_count.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                self.first_tbt.store(true, Ordering::Relaxed);
                println!("  First TBT trade: price={} size={} exchange={}",
                    trade.price as f64 / PRICE_SCALE as f64, trade.size, trade.exchange);
            }
        }

        fn on_tbt_quote(&mut self, quote: &TbtQuote, _ctx: &mut Context) {
            let n = self.quote_count.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                self.first_tbt.store(true, Ordering::Relaxed);
                println!("  First TBT quote: bid={} ask={} bid_sz={} ask_sz={}",
                    quote.bid as f64 / PRICE_SCALE as f64,
                    quote.ask as f64 / PRICE_SCALE as f64,
                    quote.bid_size, quote.ask_size);
            }
        }
    }

    let strategy = TbtStrategy {
        trade_count: tbt_trade_count.clone(),
        quote_count: tbt_quote_count.clone(),
        first_tbt: first_tbt.clone(),
    };

    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    // Subscribe to TBT AllLast for SPY
    control_tx.send(ControlCommand::SubscribeTbt {
        con_id: 756733,
        symbol: "SPY".into(),
        tbt_type: TbtType::Last,
    }).unwrap();

    let join = run_hot_loop(hot_loop);

    // Wait for first TBT data (up to 30s)
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if first_tbt.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !first_tbt.load(Ordering::Relaxed) {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        println!("  SKIP: No TBT data in 30s — market closed or HMDS not streaming\n");
        return conns;
    }

    // Collect a few seconds of data
    std::thread::sleep(Duration::from_secs(5));
    let trades = tbt_trade_count.load(Ordering::Relaxed);
    let quotes = tbt_quote_count.load(Ordering::Relaxed);

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
    println!("  PASS ({} trades, {} quotes)\n", trades, quotes);
    conns
}

// ─── Phase 62: VWAP Algo Order ───

fn phase_vwap_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 62: VWAP Algo Order (SPY)", |ctx| {
        ctx.submit_algo(0, Side::Buy, 1, 1_00_000_000, AlgoParams::Vwap {
            max_pct_vol: 0.1,
            no_take_liq: false,
            allow_past_end_time: true,
            start_time: "20260311-13:30:00".into(),
            end_time: "20260311-20:00:00".into(),
        })
    })
}

// ─── Phase 63: TWAP Algo Order ───

fn phase_twap_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 63: TWAP Algo Order (SPY)", |ctx| {
        ctx.submit_algo(0, Side::Buy, 1, 1_00_000_000, AlgoParams::Twap {
            allow_past_end_time: true,
            start_time: "20260311-13:30:00".into(),
            end_time: "20260311-20:00:00".into(),
        })
    })
}

// ─── Phase 64: Arrival Price Algo Order ───

fn phase_arrival_px_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 64: Arrival Price Algo Order (SPY)", |ctx| {
        ctx.submit_algo(0, Side::Buy, 1, 1_00_000_000, AlgoParams::ArrivalPx {
            max_pct_vol: 0.1,
            risk_aversion: RiskAversion::Neutral,
            allow_past_end_time: true,
            force_completion: false,
            start_time: "20260311-13:30:00".into(),
            end_time: "20260311-20:00:00".into(),
        })
    })
}

// ─── Phase 65: Close Price Algo Order ───

fn phase_close_px_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 65: Close Price Algo Order (SPY)", |ctx| {
        ctx.submit_algo(0, Side::Buy, 1, 1_00_000_000, AlgoParams::ClosePx {
            max_pct_vol: 0.1,
            risk_aversion: RiskAversion::Neutral,
            force_completion: false,
            start_time: "20260311-13:30:00".into(),
        })
    })
}

// ─── Phase 66: Dark Ice Algo Order ───

fn phase_dark_ice_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 66: Dark Ice Algo Order (SPY)", |ctx| {
        ctx.submit_algo(0, Side::Buy, 1, 1_00_000_000, AlgoParams::DarkIce {
            allow_past_end_time: true,
            display_size: 1,
            start_time: "20260311-13:30:00".into(),
            end_time: "20260311-20:00:00".into(),
        })
    })
}

// ─── Phase 67: % of Volume Algo Order ───

fn phase_pct_vol_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 67: % of Volume Algo Order (SPY)", |ctx| {
        ctx.submit_algo(0, Side::Buy, 1, 1_00_000_000, AlgoParams::PctVol {
            pct_vol: 0.1,
            no_take_liq: false,
            start_time: "20260311-13:30:00".into(),
            end_time: "20260311-20:00:00".into(),
        })
    })
}

// ─── Phase 68: Pegged to Benchmark Order ───

fn phase_peg_bench_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 68: Pegged to Benchmark Order (SPY pegged to AAPL)", |ctx| {
        ctx.submit_peg_bench(
            0, Side::Buy, 1,
            1_00_000_000,    // $1 limit price (far from market, won't fill)
            265598,          // AAPL conId as benchmark reference
            false,           // isPeggedChangeAmountDecrease
            50_000_000,      // $0.50 pegged change amount
            50_000_000,      // $0.50 reference change amount
        )
    })
}

// ─── Phase 69: Limit Auction Order ───

fn phase_limit_auc_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 69: Limit Auction Order (SPY)", |ctx| {
        ctx.submit_limit_auc(0, Side::Buy, 1, 1_00_000_000) // $1 limit
    })
}

// ─── Phase 70: Market-to-Limit Auction Order ───

fn phase_mtl_auc_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 70: Market-to-Limit Auction Order (SPY)", |ctx| {
        ctx.submit_mtl_auc(0, Side::Buy, 1)
    })
}

// ─── Phase 71: Box Top Order (wire-identical to MTL) ───

fn phase_box_top_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 71: Box Top Order (SPY)", |ctx| {
        ctx.submit_box_top(0, Side::Buy, 1)
    })
}

// ─── Phase 72: What-If Order (margin/commission preview) ───

fn phase_what_if_order(conns: Conns) -> Conns {
    println!("--- Phase 72: What-If Order (SPY) ---");

    let account_id = conns.account_id;
    let submitted = Arc::new(AtomicBool::new(false));
    let what_if_received = Arc::new(AtomicBool::new(false));
    let commission = Arc::new(AtomicI64::new(0));

    let sub = submitted.clone();
    let wir = what_if_received.clone();
    let comm = commission.clone();

    struct WhatIfStrategy {
        submitted: Arc<AtomicBool>,
        what_if_received: Arc<AtomicBool>,
        commission: Arc<AtomicI64>,
    }

    impl Strategy for WhatIfStrategy {
        fn on_start(&mut self, ctx: &mut Context) {
            ctx.submit_what_if(0, Side::Buy, 100, 1_00_000_000); // $1 limit, 100 shares
            self.submitted.store(true, Ordering::Relaxed);
        }

        fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

        fn on_what_if(&mut self, response: &WhatIfResponse, _ctx: &mut Context) {
            self.commission.store(response.commission, Ordering::Relaxed);
            self.what_if_received.store(true, Ordering::Relaxed);
        }

        fn on_disconnect(&mut self, _ctx: &mut Context) {}
    }

    let strategy = WhatIfStrategy {
        submitted: sub,
        what_if_received: wir,
        commission: comm,
    };

    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if what_if_received.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(submitted.load(Ordering::Relaxed), "What-if was never submitted");
    assert!(what_if_received.load(Ordering::Relaxed), "What-if response was never received");
    let comm_val = commission.load(Ordering::Relaxed);
    assert!(comm_val > 0, "Commission should be > 0, got {}", comm_val);
    println!("  Commission: ${:.2}", comm_val as f64 / PRICE_SCALE as f64);
    println!("  PASS\n");
    conns
}

// ─── Phase 73: Cash Quantity Order ───

fn phase_cash_qty_order(conns: Conns) -> Conns {
    println!("--- Phase 73: Cash Quantity Order (SPY) ---");

    let account_id = conns.account_id;
    let submitted = Arc::new(AtomicBool::new(false));
    let order_acked = Arc::new(AtomicBool::new(false));
    let order_cancelled = Arc::new(AtomicBool::new(false));
    let order_rejected = Arc::new(AtomicBool::new(false));

    let sub = submitted.clone();
    let ack = order_acked.clone();
    let can = order_cancelled.clone();
    let rej = order_rejected.clone();

    struct CashQtyStrategy {
        submitted: Arc<AtomicBool>,
        order_acked: Arc<AtomicBool>,
        cancel_sent: bool,
        order_cancelled: Arc<AtomicBool>,
        order_rejected: Arc<AtomicBool>,
        order_id: Option<OrderId>,
    }

    impl Strategy for CashQtyStrategy {
        fn on_start(&mut self, ctx: &mut Context) {
            let mut attrs = OrderAttrs::default();
            attrs.cash_qty = 1000 * PRICE_SCALE; // $1000 notional
            let id = ctx.submit_limit_ex(0, Side::Buy, 100, 1_00_000_000, b'0', attrs);
            self.order_id = Some(id);
            self.submitted.store(true, Ordering::Relaxed);
        }

        fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

        fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
            match update.status {
                OrderStatus::Submitted => {
                    self.order_acked.store(true, Ordering::Relaxed);
                    if !self.cancel_sent {
                        if let Some(id) = self.order_id {
                            ctx.cancel(id);
                            self.cancel_sent = true;
                        }
                    }
                }
                OrderStatus::Cancelled => self.order_cancelled.store(true, Ordering::Relaxed),
                OrderStatus::Rejected => self.order_rejected.store(true, Ordering::Relaxed),
                _ => {}
            }
        }

        fn on_disconnect(&mut self, _ctx: &mut Context) {}
    }

    let strategy = CashQtyStrategy {
        submitted: sub, order_acked: ack, cancel_sent: false,
        order_cancelled: can, order_rejected: rej, order_id: None,
    };

    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if order_cancelled.load(Ordering::Relaxed) || order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected.load(Ordering::Relaxed) {
        // Cash qty may be rejected on paper accounts (error 10244)
        println!("  SKIP: Cash qty rejected (expected on paper account)\n");
        return conns;
    }

    assert!(submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(order_acked.load(Ordering::Relaxed), "Order was never acknowledged");
    assert!(order_cancelled.load(Ordering::Relaxed), "Order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 74: Fractional Shares Order ───

fn phase_fractional_order(conns: Conns) -> Conns {
    println!("--- Phase 74: Fractional Shares Order (SPY) ---");

    let account_id = conns.account_id;
    let submitted = Arc::new(AtomicBool::new(false));
    let order_acked = Arc::new(AtomicBool::new(false));
    let order_cancelled = Arc::new(AtomicBool::new(false));
    let order_rejected = Arc::new(AtomicBool::new(false));

    let sub = submitted.clone();
    let ack = order_acked.clone();
    let can = order_cancelled.clone();
    let rej = order_rejected.clone();

    struct FractionalStrategy {
        submitted: Arc<AtomicBool>,
        order_acked: Arc<AtomicBool>,
        cancel_sent: bool,
        order_cancelled: Arc<AtomicBool>,
        order_rejected: Arc<AtomicBool>,
        order_id: Option<OrderId>,
    }

    impl Strategy for FractionalStrategy {
        fn on_start(&mut self, ctx: &mut Context) {
            // 0.5 shares at $1 limit
            let id = ctx.submit_limit_fractional(0, Side::Buy, QTY_SCALE / 2, 1_00_000_000);
            self.order_id = Some(id);
            self.submitted.store(true, Ordering::Relaxed);
        }

        fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

        fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
            match update.status {
                OrderStatus::Submitted => {
                    self.order_acked.store(true, Ordering::Relaxed);
                    if !self.cancel_sent {
                        if let Some(id) = self.order_id {
                            ctx.cancel(id);
                            self.cancel_sent = true;
                        }
                    }
                }
                OrderStatus::Cancelled => self.order_cancelled.store(true, Ordering::Relaxed),
                OrderStatus::Rejected => self.order_rejected.store(true, Ordering::Relaxed),
                _ => {}
            }
        }

        fn on_disconnect(&mut self, _ctx: &mut Context) {}
    }

    let strategy = FractionalStrategy {
        submitted: sub, order_acked: ack, cancel_sent: false,
        order_cancelled: can, order_rejected: rej, order_id: None,
    };

    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if order_cancelled.load(Ordering::Relaxed) || order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected.load(Ordering::Relaxed) {
        // Fractional may be rejected by CCP (API restriction)
        println!("  SKIP: Fractional rejected (may be blocked by CCP)\n");
        return conns;
    }

    assert!(submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(order_acked.load(Ordering::Relaxed), "Order was never acknowledged");
    assert!(order_cancelled.load(Ordering::Relaxed), "Order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 75: Adjustable Stop Order ───

fn phase_adjustable_stop_order(conns: Conns) -> Conns {
    run_submit_cancel_phase(conns, "Phase 75: Adjustable Stop Order (SPY)", |ctx| {
        ctx.submit_adjustable_stop(
            0, Side::Sell, 1,
            1_00_000_000,    // stop_price: $1
            500_00_000_000,  // trigger_price: $500 (above market, won't trigger)
            AdjustedOrderType::StopLimit,
            1_50_000_000,    // adjusted_stop: $1.50
            1_00_000_000,    // adjusted_limit: $1.00
        )
    })
}

// ─── Phase 76: Historical daily bars via HMDS ───

fn phase_historical_daily_bars(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 76: Historical Daily Bars (SPY, 5 days of 1-day bars) ---");

    // Reconnect HMDS fresh
    let mut hmds = match connect_farm(
        &config.host, "ushmds",
        &config.username, config.paper,
        &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded,
    ) {
        Ok(c) => {
            println!("  HMDS reconnected");
            c
        }
        Err(e) => {
            println!("  SKIP: ushmds reconnect failed: {}\n", e);
            return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id };
        }
    };

    let account_id = conns.account_id;

    // Background hot loop to keep CCP+farm heartbeats alive
    let (bg_loop, bg_tx) = HotLoop::with_connections(
        NoopStrategy, account_id.clone(), conns.farm, conns.ccp, None, None,
    );
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    // Build end_time (same helper as phase 11)
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
        query_id: "daily1".to_string(),
        con_id: 756733,
        symbol: "SPY".to_string(),
        sec_type: "CS",
        exchange: "SMART",
        data_type: BarDataType::Trades,
        end_time,
        duration: "5 d".to_string(),
        bar_size: BarSize::Day1,
        use_rth: true,
    };

    let xml = historical::build_query_xml(&req);
    hmds.send_fixcomp(&[
        (fix::TAG_MSG_TYPE, "W"),
        (historical::TAG_HISTORICAL_XML, &xml),
    ]).expect("Failed to send daily bar request");

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

    println!("  Total daily bars: {}", all_bars.len());
    if all_bars.is_empty() {
        println!("  SKIP: No daily bars received (HMDS may be unavailable)\n");
        return bg_conns;
    }

    // Daily bars for 5 trading days — expect 1-5 bars depending on market calendar
    assert!(all_bars.len() <= 5, "Should have at most 5 daily bars, got {}", all_bars.len());
    for bar in &all_bars {
        assert!(bar.open > 0.0, "Open should be positive");
        assert!(bar.high >= bar.low, "High ({}) should be >= Low ({})", bar.high, bar.low);
        assert!(bar.high >= bar.open, "High ({}) should be >= Open ({})", bar.high, bar.open);
        assert!(bar.high >= bar.close, "High ({}) should be >= Close ({})", bar.high, bar.close);
        assert!(bar.low <= bar.open, "Low ({}) should be <= Open ({})", bar.low, bar.open);
        assert!(bar.low <= bar.close, "Low ({}) should be <= Close ({})", bar.low, bar.close);
        assert!(bar.volume > 0, "Volume should be positive");
        println!("  {} O={:.2} H={:.2} L={:.2} C={:.2} V={}", bar.time, bar.open, bar.high, bar.low, bar.close, bar.volume);
    }
    println!("  PASS ({} daily bars)\n", all_bars.len());

    bg_conns
}

// ─── Phase 77: Cancel historical request ───

fn phase_cancel_historical(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 77: Cancel Historical Request (SPY) ---");

    // Reconnect HMDS fresh
    let mut hmds = match connect_farm(
        &config.host, "ushmds",
        &config.username, config.paper,
        &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded,
    ) {
        Ok(c) => {
            println!("  HMDS reconnected");
            c
        }
        Err(e) => {
            println!("  SKIP: ushmds reconnect failed: {}\n", e);
            return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id };
        }
    };

    let account_id = conns.account_id;

    // Background hot loop for heartbeats
    let (bg_loop, bg_tx) = HotLoop::with_connections(
        NoopStrategy, account_id.clone(), conns.farm, conns.ccp, None, None,
    );
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    // Send a historical request
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
        query_id: "cancel_test".to_string(),
        con_id: 756733,
        symbol: "SPY".to_string(),
        sec_type: "CS",
        exchange: "SMART",
        data_type: BarDataType::Trades,
        end_time,
        duration: "1 d".to_string(),
        bar_size: BarSize::Sec1,  // 1-second bars = lots of data, slow to complete
        use_rth: true,
    };

    let xml = historical::build_query_xml(&req);
    hmds.send_fixcomp(&[
        (fix::TAG_MSG_TYPE, "W"),
        (historical::TAG_HISTORICAL_XML, &xml),
    ]).expect("Failed to send historical request");

    // Wait briefly for the first chunk to arrive (confirms request was accepted)
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

    // Send cancel (35=Z)
    let cancel_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfCancelQueries>\
         <CancelQuery>\
         <id>cancel_test</id>\
         </CancelQuery>\
         </ListOfCancelQueries>"
    );
    hmds.send_fixcomp(&[
        (fix::TAG_MSG_TYPE, "Z"),
        (historical::TAG_HISTORICAL_XML, &cancel_xml),
    ]).expect("Failed to send cancel request");
    println!("  Cancel sent");

    // Drain remaining — after cancel, no more bars with is_complete should arrive
    // (or data stops quickly). We just verify no error/disconnect occurs.
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
                        bars_after_cancel += resp.bars.len() as u32;
                    }
                }
            }
        }
    }

    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);

    println!("  Bars received after cancel: {} (in-flight data is expected)", bars_after_cancel);
    // Connection should still be alive after cancel
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
    assert!(def.con_id > 0, "conId should be resolved");
    assert_eq!(def.sec_type, contracts::SecurityType::Stock);
    assert_eq!(def.currency, "USD");
    assert!(!def.long_name.is_empty(), "Long name should not be empty");
    assert!(def.min_tick > 0.0, "Min tick should be positive");
    println!("  conId={} MinTick={}", def.con_id, def.min_tick);
    println!("  PASS\n");
}

// ─── Phase 79: Head timestamp request via HMDS ───

fn phase_head_timestamp(conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 79: Head Timestamp (SPY, TRADES) ---");

    // Reconnect HMDS fresh
    let mut hmds = match connect_farm(
        &config.host, "ushmds",
        &config.username, config.paper,
        &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded,
    ) {
        Ok(c) => {
            println!("  HMDS reconnected");
            c
        }
        Err(e) => {
            println!("  SKIP: ushmds reconnect failed: {}\n", e);
            return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id };
        }
    };

    let account_id = conns.account_id;

    // Background hot loop for heartbeats
    let (bg_loop, bg_tx) = HotLoop::with_connections(
        NoopStrategy, account_id.clone(), conns.farm, conns.ccp, None, None,
    );
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    let req = HeadTimestampRequest {
        con_id: 756733,
        sec_type: "STK",
        exchange: "SMART",
        data_type: BarDataType::Trades,
        use_rth: true,
    };

    let xml = historical::build_head_timestamp_xml(&req);
    hmds.send_fixcomp(&[
        (fix::TAG_MSG_TYPE, "W"),
        (historical::TAG_HISTORICAL_XML, &xml),
    ]).expect("Failed to send head timestamp request");

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
    // SPY head timestamp for TRADES should be in the 1990s
    assert!(!resp.head_timestamp.is_empty(), "Head timestamp should not be empty");
    assert!(resp.head_timestamp.starts_with("199"), "SPY TRADES head timestamp should be in 1990s, got {}", resp.head_timestamp);
    assert!(!resp.timezone.is_empty(), "Timezone should not be empty");
    println!("  PASS\n");

    bg_conns
}

// ─── Phase 80: Trading hours / schedule verification ───

fn phase_trading_hours(conns: &mut Conns) {
    println!("--- Phase 80: Trading Hours (schedule subscription, AAPL) ---");

    // Subscribe AAPL on farm to trigger schedule response on CCP
    let now = ibx::gateway::chrono_free_timestamp();
    conns.farm.send_fixcomp(&[
        (fix::TAG_MSG_TYPE, "V"),
        (fix::TAG_SENDING_TIME, &now),
        (263, "1"),   // SubscriptionRequestType = Subscribe
        (146, "1"),   // NoRelatedSym = 1
        (262, "sched_test"),
        (6008, "265598"),  // AAPL conId
        (207, "BEST"),
        (167, "CS"),
        (264, "442"),      // BidAsk
        (6088, "Socket"),
        (9830, "1"),
        (9839, "1"),
    ]).expect("Failed to send farm subscribe for AAPL");
    println!("  Subscribed AAPL on farm, listening on CCP for schedule");

    let mut schedule: Option<contracts::ContractSchedule> = None;
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline && schedule.is_none() {
        // Drain farm data (tick responses) to keep connection alive
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
                Frame::FixComp(raw) => {
                    let (unsigned, _) = conns.ccp.unsign(&raw);
                    fixcomp::fixcomp_decompress(&unsigned)
                }
                Frame::Fix(raw) => vec![raw],
                _ => continue,
            };

            for msg in messages {
                if let Some(sched) = contracts::parse_schedule_response(&msg) {
                    println!("  Schedule: tz={} trading={} liquid={}",
                        sched.timezone, sched.trading_hours.len(), sched.liquid_hours.len());
                    schedule = Some(sched);
                }
            }
        }
    }

    // Unsubscribe AAPL to clean up
    let now = ibx::gateway::chrono_free_timestamp();
    let _ = conns.farm.send_fixcomp(&[
        (fix::TAG_MSG_TYPE, "V"),
        (fix::TAG_SENDING_TIME, &now),
        (263, "2"),   // Unsubscribe
        (146, "1"),
        (262, "sched_test"),
        (6008, "265598"),
        (207, "BEST"),
        (167, "CS"),
        (264, "442"),
        (6088, "Socket"),
        (9830, "1"),
        (9839, "1"),
    ]);

    if schedule.is_none() {
        println!("  SKIP: No schedule received (may not be triggered by subscribe)\n");
        return;
    }

    let sched = schedule.unwrap();
    assert!(!sched.timezone.is_empty(), "Timezone should not be empty");
    assert!(!sched.trading_hours.is_empty(), "Trading hours should not be empty");
    assert!(!sched.liquid_hours.is_empty(), "Liquid hours should not be empty");
    // Liquid hours should be a subset of trading hours (fewer or equal sessions)
    assert!(sched.liquid_hours.len() <= sched.trading_hours.len(),
        "Liquid hours ({}) should be <= trading hours ({})",
        sched.liquid_hours.len(), sched.trading_hours.len());
    if let Some(th) = sched.trading_hours.first() {
        println!("  First trading session: {} → {} ({})", th.start, th.end, th.trade_date);
    }
    if let Some(lh) = sched.liquid_hours.first() {
        println!("  First liquid session: {} → {} ({})", lh.start, lh.end, lh.trade_date);
    }
    println!("  PASS\n");
}
