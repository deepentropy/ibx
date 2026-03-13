//! Shared types and helpers for integration tests.

use std::env;

pub(super) use std::sync::Arc;
pub(super) use std::time::{Duration, Instant};

pub(super) use ibx::bridge::{Event, SharedState};
pub(super) use ibx::engine::hot_loop::HotLoop;
pub(super) use ibx::gateway::{self, GatewayConfig};
pub(super) use ibx::protocol::connection::{Connection, Frame};
pub(super) use ibx::protocol::{fix, fixcomp};
pub(super) use ibx::types::*;

pub(super) fn get_config() -> Option<GatewayConfig> {
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
pub(super) struct Conns {
    pub(super) farm: Connection,
    pub(super) ccp: Connection,
    pub(super) hmds: Option<Connection>,
    pub(super) account_id: String,
}

/// Run a hot loop in a background thread, returning the HotLoop for connection reclamation.
pub(super) fn run_hot_loop(hot_loop: HotLoop) -> std::thread::JoinHandle<HotLoop> {
    std::thread::spawn(move || {
        let mut hl = hot_loop;
        hl.run();
        hl
    })
}

/// Shutdown a hot loop and reclaim connections.
pub(super) fn shutdown_and_reclaim(
    control_tx: &crossbeam_channel::Sender<ControlCommand>,
    join: std::thread::JoinHandle<HotLoop>,
    account_id: String,
) -> Conns {
    let _ = control_tx.send(ControlCommand::Shutdown);
    let mut hl = join.join().expect("hot loop thread panicked");
    let farm = hl.farm_conn.take().expect("farm_conn missing after shutdown");
    let mut ccp = hl.ccp_conn.take().expect("ccp_conn missing after shutdown");
    let hmds = hl.hmds_conn.take();

    // Keep CCP alive between phases — drain pending data and send heartbeat.
    // Without this, IB kills the CCP connection if we're idle for >10s during
    // phase transitions (e.g., HMDS reconnection in historical phases).
    ccp_keepalive(&mut ccp);

    Conns { farm, ccp, hmds, account_id }
}

/// Send a heartbeat on CCP and respond to any pending TestRequests.
/// Prevents IB from killing the connection during phase transitions.
pub(super) fn ccp_keepalive(ccp: &mut Connection) {
    // Drain any pending data (heartbeats, TestRequests from IB)
    let _ = ccp.try_recv();
    if ccp.has_buffered_data() || true {
        let frames = ccp.extract_frames();
        for frame in frames {
            let raw = match &frame {
                Frame::Fix(r) | Frame::FixComp(r) | Frame::Binary(r) => r,
            };
            let (unsigned, _) = ccp.unsign(raw);
            let msg = if matches!(frame, Frame::FixComp(_)) {
                fixcomp::fixcomp_decompress(&unsigned).into_iter().next()
            } else {
                Some(unsigned)
            };
            if let Some(m) = msg {
                let parsed = fix::fix_parse(&m);
                if parsed.get(&fix::TAG_MSG_TYPE).map(|s| s.as_str()) == Some(fix::MSG_TEST_REQUEST) {
                    // Respond to TestRequest with Heartbeat containing the test ID
                    let test_id = parsed.get(&fix::TAG_TEST_REQ_ID).cloned().unwrap_or_default();
                    let ts = gateway::chrono_free_timestamp();
                    let _ = ccp.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                }
            }
        }
    }

    // Send our own heartbeat
    let ts = gateway::chrono_free_timestamp();
    let _ = ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
        (fix::TAG_SENDING_TIME, &ts),
    ]);
}

/// Generate a unique order ID based on current time.
pub(super) fn next_order_id() -> OrderId {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64 * 1000
}

// ─── Market session detection ───

/// US stock market session based on current Eastern Time.
/// DST: second Sunday of March (spring forward) to first Sunday of November (fall back).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum MarketSession {
    Regular,    // Mon-Fri 9:30-16:00 ET
    PreMarket,  // Mon-Fri 4:00-9:30 ET
    AfterHours, // Mon-Fri 16:00-20:00 ET
    Closed,     // Mon-Fri 20:00-4:00 ET, weekends
}

pub(super) fn market_session() -> (MarketSession, u16) {
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

// ─── Generic submit+cancel helper ───
// fill_or_cancel=false: only cancelled counts as success
// fill_or_cancel=true: filled OR cancelled both count as success

pub(super) fn run_submit_cancel_phase(
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

// ─── Timestamp helper (shared by historical phases) ───

pub(super) fn now_ib_timestamp() -> String {
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

/// Format seconds since epoch as YYYYMMDD-HH:MM:SS UTC.
pub(super) fn format_utc_timestamp(epoch_secs: u64) -> String {
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
