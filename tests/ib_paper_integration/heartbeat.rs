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

    assert!(!disconnected, "Farm disconnected after {:.1}s — heartbeat failed", elapsed.as_secs_f64());
    println!("  PASS ({:.1}s, no disconnect, survived 2x farm heartbeat interval)\n", elapsed.as_secs_f64());
    conns
}

pub(super) fn phase_heartbeat_timeout_detection(conns: Conns) -> Conns {
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
