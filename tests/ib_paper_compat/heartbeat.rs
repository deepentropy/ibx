//! Heartbeat keepalive and timeout detection test phases.

use super::common::*;
use std::net::TcpListener;

pub(super) fn phase_heartbeat_keepalive(conns: Conns) -> Conns {
    println!("--- Phase 13: Heartbeat Keepalive (20s > CCP 10s interval) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into(), exchange: String::new(), sec_type: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(), mode_9887: 0, reply_tx: None }).unwrap();
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

    check!(!disconnected, "Connection dropped after {:.1}s — heartbeat mechanism failed", elapsed.as_secs_f64());
    println!("  PASS ({:.1}s, no disconnect)\n", elapsed.as_secs_f64());
    conns
}

pub(super) fn phase_farm_heartbeat_keepalive(conns: Conns) -> Conns {
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

    check!(!disconnected, "Farm disconnected after {:.1}s — heartbeat failed", elapsed.as_secs_f64());
    println!("  PASS ({:.1}s, no disconnect, survived 2x farm heartbeat interval)\n", elapsed.as_secs_f64());
    conns
}

pub(super) fn phase_heartbeat_timeout_detection(conns: Conns) -> Conns {
    use ibx::engine::hot_loop::liveness::{CCP_DEAD_LIMIT, CCP_SEND_PAUSE};
    println!("--- Phase 56: Heartbeat Timeout Detection (simulated stale CCP) ---");

    let account_id = conns.account_id;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind localhost");
    let addr = listener.local_addr().unwrap();
    let client = std::net::TcpStream::connect(addr).expect("connect to localhost");
    let _server = listener.accept().expect("accept dead socket").0;
    let dead_ccp = Connection::new_raw(client).expect("wrap dead socket as Connection");
    // The real session is not used by the loop of this phase; it is kept
    // alive meanwhile, or the server stops serving it during the wait and
    // the next phases run on a session that no longer answers.
    let mut real_ccp = conns.ccp;

    // The engine's own limits (ibx#419): the start of the loop counts as the
    // last send on the dead link, every send starts a pause in which the
    // link is not checked, and the first check after it finds the silence.
    let earliest = CCP_SEND_PAUSE;
    let latest = CCP_DEAD_LIMIT + Duration::from_secs(2);

    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, dead_ccp, conns.hmds, None,
    );
    let join = run_hot_loop(hot_loop);

    let start = Instant::now();
    let mut lost_at = None;
    let mut session_closed = false;
    let mut last_keepalive = Instant::now();
    while start.elapsed() < latest + Duration::from_secs(5) {
        if last_keepalive.elapsed() >= Duration::from_secs(5) {
            ccp_keepalive(&mut real_ccp);
            last_keepalive = Instant::now();
        }
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, Event::Disconnected) {
                session_closed = true;
            }
        }
        if shared.drain_connection_notices().iter().any(|(code, _)| *code == 1100) {
            lost_at = Some(start.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let elapsed = start.elapsed();
    check!(lost_at.is_some(), "No 1100 after {:.1}s — a silent auth link must be reset within {:?}",
        elapsed.as_secs_f64(), latest);
    if let Some(at) = lost_at {
        check!(at >= earliest && at <= latest,
            "1100 at {:.1}s — expected between {:?} and {:?}", at.as_secs_f64(), earliest, latest);
    }
    check!(!session_closed, "The session was closed — a lost link must leave the clients connected");

    let reclaimed = shutdown_and_reclaim(&control_tx, join, account_id.clone());
    ccp_keepalive(&mut real_ccp);

    println!("  1100 at {:?} (expected {:?} to {:?})", lost_at, earliest, latest);
    println!("  Loop survived the reset (graceful shutdown succeeded)");
    println!("  PASS
");

    Conns { farm: reclaimed.farm, ccp: real_ccp, hmds: reclaimed.hmds, account_id }
}
