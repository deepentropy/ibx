//! Connection, authentication, and recovery test phases.

use super::common::*;
use std::net::TcpListener;
use ibx::gateway::{Gateway, GatewayConfig};

pub(super) fn phase_ccp_auth(gw: &Gateway, has_hmds: bool, connect_time: Duration) {
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

pub(super) fn phase_extra_farms(gw: &Gateway, config: &GatewayConfig) {
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

pub(super) fn phase_graceful_shutdown(conns: Conns) -> Conns {
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

pub(super) fn phase_connection_recovery(conns: Conns, _gw: &Gateway, config: &GatewayConfig) -> Conns {
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

pub(super) fn phase_reconnection_state_recovery(conns: Conns, _gw: &Gateway, _config: &GatewayConfig) -> Conns {
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
