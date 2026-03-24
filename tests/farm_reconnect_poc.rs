//! POC: Validate farm auto-reconnect with cached session credentials.
//!
//! Test 1: connect_farm() works with cached K (no SRP) — proves the thesis
//! Test 2: HotLoop auto-reconnect fires after farm disconnect — end-to-end

use std::sync::Arc;
use std::time::{Duration, Instant};

use ibx::bridge::SharedState;
use ibx::gateway::{connect_farm, Gateway, GatewayConfig};

fn config() -> GatewayConfig {
    GatewayConfig {
        username: std::env::var("IB_USERNAME").expect("IB_USERNAME"),
        password: std::env::var("IB_PASSWORD").expect("IB_PASSWORD"),
        host: std::env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string()),
        paper: true,
        accept_invalid_certs: false,
    }
}

#[test]
fn farm_reconnect_with_cached_credentials() {
    let cfg = config();

    // Phase 1: Full auth
    let t0 = Instant::now();
    let (gw, farm_conn, _ccp_conn, _hmds, _cash, _usfut, _eu, _j) =
        Gateway::connect(&cfg).expect("Initial connect failed");
    let full_auth_ms = t0.elapsed().as_millis();

    // Save credentials
    let session_key = gw.session_token.clone();
    let server_session_id = gw.server_session_id.clone();
    let hw_info = gw.hw_info.clone();
    let encoded = gw.encoded.clone();

    println!("Full auth: {}ms | Account: {}", full_auth_ms, gw.account_id);

    // Phase 2: Drop original farm connection
    drop(farm_conn);

    // Phase 3: Reconnect using cached credentials (no SRP)
    let t1 = Instant::now();
    let new_farm = connect_farm(
        &cfg.host, "usfarm",
        &cfg.username, cfg.paper,
        &server_session_id, &session_key, &hw_info, &encoded,
    ).expect("Farm reconnect with cached credentials FAILED");
    let reconnect_ms = t1.elapsed().as_millis();

    println!("Farm reconnect: {}ms (no SRP) | seq={}", reconnect_ms, new_farm.seq);
    assert!(new_farm.seq > 0);
    println!("PASS: cached K reconnect works, {:.1}x speedup", full_auth_ms as f64 / reconnect_ms.max(1) as f64);
}

#[test]
fn hotloop_auto_reconnect_on_farm_disconnect() {
    let cfg = config();

    let (gw, farm_conn, ccp_conn, hmds, cash, usfut, eu, j) =
        Gateway::connect(&cfg).expect("Initial connect failed");

    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::bounded(256);

    let (mut hot_loop, _control_tx) = gw.into_hot_loop_with_farms(
        shared.clone(), Some(event_tx),
        farm_conn, ccp_conn, hmds, cash, usfut, eu, j, None,
    );
    hot_loop.update_reconnect_auth(cfg.host.clone(), cfg.username.clone(), cfg.paper);
    println!("Reconnect auth set: host={}, user={}, paper={}", cfg.host, cfg.username, cfg.paper);

    assert!(!hot_loop.is_farm_disconnected());

    // Run a few iterations to process initial data
    for _ in 0..100 {
        hot_loop.poll_once();
    }
    assert!(!hot_loop.is_farm_disconnected());

    // Force farm disconnect by dropping the connection
    hot_loop.farm_conn = None;
    hot_loop.force_farm_disconnect();

    assert!(hot_loop.is_farm_disconnected());
    println!("Farm disconnected, spawning auto-reconnect...");

    // Trigger reconnect spawn
    hot_loop.spawn_farm_reconnect_for_test();
    println!("Reconnect thread spawned, polling for result...");

    // Poll until reconnect completes (up to 60s — connect_farm takes ~7s)
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut polls = 0u32;
    while hot_loop.is_farm_disconnected() && Instant::now() < deadline {
        hot_loop.poll_farm_reconnect_for_test();
        polls += 1;
        if polls % 50 == 0 {
            println!("  ...still waiting ({:.0}s elapsed)", Instant::now().duration_since(deadline - Duration::from_secs(60)).as_secs_f64());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(!hot_loop.is_farm_disconnected(), "Farm should have reconnected within 60s");
    assert!(hot_loop.farm_conn.is_some(), "Farm connection should be restored");
    println!("PASS: HotLoop auto-reconnected farm after disconnect");
}
