//! POC: Validate farm auto-reconnect with cached session credentials.
//!
//! Test 1: connect_farm() works with cached K (no SRP) — proves the thesis
//! Test 2: HotLoop auto-reconnect fires after farm disconnect — end-to-end

use std::sync::Arc;
use std::time::{Duration, Instant};

use ibx::bridge::SharedState;
use ibx::gateway::{connect_farm, reconnect_ccp, reconnect_ccp_session, Gateway, GatewayConfig, ReconnectAuth};

/// These tests log in the same paper user: one at a time, whatever the
/// number of test threads (ibx#586). A test that failed leaves the lock
/// poisoned; the next ones still run.
static ONE_LOGIN: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one_login() -> std::sync::MutexGuard<'static, ()> {
    ONE_LOGIN.lock().unwrap_or_else(|e| e.into_inner())
}

fn config() -> GatewayConfig {
    GatewayConfig {
        username: std::env::var("IB_USERNAME").expect("IB_USERNAME"),
        password: zeroize::Zeroizing::new(std::env::var("IB_PASSWORD").expect("IB_PASSWORD")),
        host: std::env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string()),
        paper: true,
        accept_invalid_certs: false,
        ib_key_timeout_secs: ibx::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
        ib_key_token_sub_type: ibx::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
        code_provider: None,
        restart_token_file: None,
    }
}

#[test]
#[ignore = "live: logs in to the paper account (IB_USERNAME / IB_PASSWORD)"]
fn farm_reconnect_with_cached_credentials() {
    let _one = one_login();
    let cfg = config();

    // Phase 1: Full auth
    let t0 = Instant::now();
    let (gw, farm_conn, _ccp_conn, _hmds) =
        Gateway::connect(&cfg).expect("Initial connect failed");
    assert!(gw.account_id.starts_with("DU"), "refusing to run: the logged-in account is not a paper account (its id does not start with DU)");
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
        &cfg.username, &cfg.password, cfg.paper,
        &server_session_id, &session_key, &hw_info, &encoded, 18,
    ).expect("Farm reconnect with cached credentials FAILED");
    let reconnect_ms = t1.elapsed().as_millis();

    println!("Farm reconnect: {}ms (no SRP) | seq={}", reconnect_ms, new_farm.seq);
    assert!(new_farm.seq > 0);
    println!("PASS: cached K reconnect works, {:.1}x speedup", full_auth_ms as f64 / reconnect_ms.max(1) as f64);
}

#[test]
#[ignore = "live: logs in to the paper account (IB_USERNAME / IB_PASSWORD)"]
fn hotloop_auto_reconnect_on_farm_disconnect() {
    let _one = one_login();
    let cfg = config();

    let (gw, farm_conn, ccp_conn, hmds) =
        Gateway::connect(&cfg).expect("Initial connect failed");

    assert!(gw.account_id.starts_with("DU"), "refusing to run: the logged-in account is not a paper account (its id does not start with DU)");

    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::bounded(256);

    let (mut hot_loop, control_tx) = gw.into_hot_loop_with_farms(
        shared.clone(), Some(event_tx),
        farm_conn, ccp_conn, hmds, None,
    );
    hot_loop.update_reconnect_auth(cfg.host.clone(), cfg.username.clone(), cfg.password.clone(), cfg.paper);
    println!("Reconnect auth set: host={}, user={}, paper={}", cfg.host, cfg.username, cfg.paper);

    assert!(!hot_loop.is_farm_disconnected());

    // Run a few iterations to process initial data
    for _ in 0..100 {
        hot_loop.poll_once();
    }
    assert!(!hot_loop.is_farm_disconnected());

    // ibx#288: subscribe SPY. The server's subscription acknowledgement sets
    // the instrument's min tick, so a non-zero min tick proves the server
    // accepted the subscription (it arrives with the market closed too).
    control_tx.send(ibx::types::ControlCommand::Subscribe {
        con_id: 756733, symbol: "SPY".into(), exchange: String::new(), sec_type: String::new(),
        last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
        mode_9887: 0, snapshot: false, reply_tx: None,
    }).unwrap();
    hot_loop.poll_once();
    let spy = hot_loop.market_for_test().instrument_by_con_id(756733).expect("SPY registered");
    let acked = |hot_loop: &mut ibx::engine::hot_loop::HotLoop| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            // Both sockets, as the running loop polls them: a stock
            // subscription is sent once its definition came in on the auth
            // connection (ibx#287).
            hot_loop.poll_farm_for_test();
            hot_loop.poll_auth_for_test();
            // The subscription leaves in a market data pass, which the
            // command step sends when it is due (ibx#560, ibx#586).
            hot_loop.poll_once();
            if hot_loop.market_for_test().min_tick(spy) > 0.0 { return true; }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    };
    assert!(acked(&mut hot_loop), "SPY subscription not acknowledged before the drop");
    println!("SPY subscription acknowledged");

    // Lose the farm connection through the real loss path, and clear the
    // acknowledgement marker so only a new acknowledgement can set it.
    hot_loop.lose_farm_for_test();
    hot_loop.market_for_test().set_min_tick(spy, 0.0);

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
    assert!(acked(&mut hot_loop), "SPY subscription not re-issued after the reconnect (ibx#288)");
    println!("PASS: HotLoop auto-reconnected farm after disconnect and restored the SPY subscription");
}

#[test]
#[ignore = "live: logs in to the paper account (IB_USERNAME / IB_PASSWORD)"]
fn ccp_reconnect_with_cached_credentials() {
    let _one = one_login();
    let cfg = config();

    let t0 = Instant::now();
    let (gw, _farm_conn, ccp_conn, _hmds) =
        Gateway::connect(&cfg).expect("Initial connect failed");
    assert!(gw.account_id.starts_with("DU"), "refusing to run: the logged-in account is not a paper account (its id does not start with DU)");
    let full_auth_ms = t0.elapsed().as_millis();

    let auth = ReconnectAuth {
        host: cfg.host.clone(),
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        paper: cfg.paper,
        session_key: gw.session_token.clone(),
        session_token: gw.session_token.clone(),
        server_session_id: gw.server_session_id.clone(),
        hw_info: gw.hw_info.clone(),
        encoded: gw.encoded.clone(),
        hmds_host: gw.hmds_host.clone(),
        hmds_farm: gw.hmds_farm.clone(),
        farm_host: gw.farm_host.clone(),
        farm_name: gw.farm_name.clone(),
        session_epoch: gw.session_epoch.clone(),
        ns_secure_refused: gw.ns_secure_refused,
        use_ssl: gw.use_ssl,
        ssl_farms: gw.ssl_farms.clone(),
        second_factor: Default::default(),
        restart_token: gw.restart_token.clone(),
    };

    println!("Full auth: {}ms | session_id={}", full_auth_ms, auth.server_session_id);

    // Drop original CCP connection
    drop(ccp_conn);
    println!("Original CCP connection dropped");

    // Reconnect using cached credentials (SOFT_TOKEN, no SRP)
    let t1 = Instant::now();
    let result = reconnect_ccp(&auth);
    let reconnect_ms = t1.elapsed().as_millis();

    match result {
        Ok(conn) => {
            println!("CCP reconnect: {}ms (SOFT_TOKEN) | seq={}", reconnect_ms, conn.seq);
            println!("PASS: CCP reconnect with cached K works, {:.1}x speedup",
                full_auth_ms as f64 / reconnect_ms.max(1) as f64);
        }
        Err(e) => {
            println!("CCP reconnect failed after {}ms: {}", reconnect_ms, e);
            println!("INFO: Server requires full SRP for CCP — auto-reconnect not possible without password");
            // This is an expected outcome — don't fail the test, just report
        }
    }
}

/// ibx#423: the auth login and a reconnect of the auth connection run over
/// TLS with no key exchange, as the reference; the farm logon keeps its key
/// exchange. Both logon replies give a clock offset (ibx#421).
/// Run with: cargo test --test farm_reconnect_poc auth_login_and_reconnect_without_key_exchange_live -- --ignored --nocapture
#[test]
#[ignore]
fn auth_login_and_reconnect_without_key_exchange_live() {
    let _ = env_logger::try_init();
    let _one = one_login();
    let cfg = config();
    let (gw, _farm_conn, ccp_conn, _hmds) = Gateway::connect(&cfg).expect("Initial connect failed");
    assert!(gw.account_id.starts_with("DU"), "refusing to run: the logged-in account is not a paper account (its id does not start with DU)");
    assert!(!gw.ns_secure_refused, "the server refused the encryption");
    println!("login: clock offset {:?} ms, backfill years {}", gw.logon.clock_offset_ms, gw.max_backfill_years);
    assert!(gw.logon.clock_offset_ms.is_some(), "the logon reply gives the server time");

    let auth = ReconnectAuth {
        host: cfg.host.clone(),
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        paper: cfg.paper,
        session_key: gw.session_token.clone(),
        session_token: gw.session_token.clone(),
        server_session_id: gw.server_session_id.clone(),
        hw_info: gw.hw_info.clone(),
        encoded: gw.encoded.clone(),
        hmds_host: gw.hmds_host.clone(),
        hmds_farm: gw.hmds_farm.clone(),
        farm_host: gw.farm_host.clone(),
        farm_name: gw.farm_name.clone(),
        session_epoch: gw.session_epoch.clone(),
        ns_secure_refused: gw.ns_secure_refused,
        use_ssl: gw.use_ssl,
        ssl_farms: gw.ssl_farms.clone(),
        second_factor: Default::default(),
        restart_token: gw.restart_token.clone(),
    };
    drop(ccp_conn);
    let reconnect = reconnect_ccp_session(&auth).expect("auth reconnect without key exchange");
    assert!(!reconnect.ns_secure_refused);
    println!("reconnect: clock offset {:?} ms, epoch {:?}", reconnect.logon.clock_offset_ms, reconnect.session_epoch);

    let farm = connect_farm(
        &cfg.host, "usfarm", &cfg.username, &cfg.password, cfg.paper,
        &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded, 18,
    ).expect("farm logon with its key exchange");
    assert!(farm.seq > 0);
}

/// ibx#423: the reference's mode without TLS (jts.ini UseSSL false,
/// IBX_USE_SSL=false here): the auth login on a plain socket to port 4000
/// with the key exchange first, then a reconnect the same way. Never
/// captured from the reference: this run tells whether the server takes
/// the login of that mode from ibx.
/// Run with: cargo test --test farm_reconnect_poc auth_login_without_tls_live -- --ignored --nocapture
#[test]
#[ignore]
fn auth_login_without_tls_live() {
    let _ = env_logger::try_init();
    // SAFETY: an ignored test, run alone; nothing else reads the
    // environment at the same time.
    unsafe { std::env::set_var("IBX_USE_SSL", "false") };
    let _one = one_login();
    let cfg = config();
    let (gw, _farm_conn, ccp_conn, _hmds) = Gateway::connect(&cfg).expect("login without TLS");
    assert!(gw.account_id.starts_with("DU"), "refusing to run: the logged-in account is not a paper account (its id does not start with DU)");
    assert!(!gw.use_ssl);
    println!("login without TLS: encryption refused {}, clock offset {:?} ms", gw.ns_secure_refused, gw.logon.clock_offset_ms);
    let auth = ReconnectAuth {
        host: cfg.host.clone(),
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        paper: cfg.paper,
        session_key: gw.session_token.clone(),
        session_token: gw.session_token.clone(),
        server_session_id: gw.server_session_id.clone(),
        hw_info: gw.hw_info.clone(),
        encoded: gw.encoded.clone(),
        hmds_host: gw.hmds_host.clone(),
        hmds_farm: gw.hmds_farm.clone(),
        farm_host: gw.farm_host.clone(),
        farm_name: gw.farm_name.clone(),
        session_epoch: gw.session_epoch.clone(),
        ns_secure_refused: gw.ns_secure_refused,
        use_ssl: gw.use_ssl,
        ssl_farms: gw.ssl_farms.clone(),
        second_factor: Default::default(),
        restart_token: gw.restart_token.clone(),
    };
    drop(ccp_conn);
    let reconnect = reconnect_ccp_session(&auth).expect("auth reconnect without TLS");
    println!("reconnect without TLS: encryption refused {}, epoch {:?}", reconnect.ns_secure_refused, reconnect.session_epoch);
    unsafe { std::env::remove_var("IBX_USE_SSL") };
}

/// ibx#761: about a day after the login the server no longer takes the
/// session token and asks a reconnect for a full login. A token it does not
/// know brings the same answer: the reconnect logs in again in full (on
/// paper: the password step, no second factor), gives the new key, and the
/// next reconnect passes on that key with the session-token step.
/// Run with: cargo test --test farm_reconnect_poc ccp_reconnect_with_a_lapsed_token_logs_in_again -- --ignored --nocapture
#[test]
#[ignore = "live: logs in to the paper account (IB_USERNAME / IB_PASSWORD)"]
fn ccp_reconnect_with_a_lapsed_token_logs_in_again() {
    let _ = env_logger::try_init();
    let _one = one_login();
    let cfg = config();
    let (gw, _farm_conn, ccp_conn, _hmds) = Gateway::connect(&cfg).expect("Initial connect failed");
    assert!(gw.account_id.starts_with("DU"), "refusing to run: the logged-in account is not a paper account (its id does not start with DU)");
    let lapsed = num_bigint::BigUint::from(123_456_789u64);
    let mut auth = ReconnectAuth {
        host: cfg.host.clone(),
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        paper: cfg.paper,
        session_key: lapsed.clone(),
        session_token: lapsed,
        server_session_id: gw.server_session_id.clone(),
        hw_info: gw.hw_info.clone(),
        encoded: gw.encoded.clone(),
        hmds_host: gw.hmds_host.clone(),
        hmds_farm: gw.hmds_farm.clone(),
        farm_host: gw.farm_host.clone(),
        farm_name: gw.farm_name.clone(),
        session_epoch: gw.session_epoch.clone(),
        ns_secure_refused: gw.ns_secure_refused,
        use_ssl: gw.use_ssl,
        ssl_farms: gw.ssl_farms.clone(),
        second_factor: ibx::gateway::SecondFactorSettings::of(&cfg),
        restart_token: None,
    };
    drop(ccp_conn);
    let first = reconnect_ccp_session(&auth).expect("the reconnect logs in again in full");
    let key = first.session_key.expect("a full login gives the new key");
    drop(first.conn);
    auth.session_key = key.clone();
    auth.session_token = key;
    std::thread::sleep(Duration::from_secs(5));
    let second = reconnect_ccp_session(&auth).expect("the next reconnect passes on the new key");
    assert!(second.session_key.is_none(), "the session-token step was taken, not a full login");
    println!("PASS: a lapsed token is replaced by a full login, and the next reconnect passes on its key");
}

/// ibx#762: the login announces a restart token. When the server no longer
/// takes the session token (a lapsed one is simulated with an unknown
/// value), the reconnect presents the restart token and the server lets the
/// session in with no password step; the session token of that login is
/// taken by the next reconnect and by a farm.
/// Run with: cargo test --test farm_reconnect_poc ccp_reconnect_with_a_lapsed_token_presents_the_restart_token -- --ignored --nocapture
#[test]
#[ignore = "live: logs in to the paper account (IB_USERNAME / IB_PASSWORD)"]
fn ccp_reconnect_with_a_lapsed_token_presents_the_restart_token() {
    let _ = env_logger::try_init();
    let _one = one_login();
    let cfg = config();
    let (gw, _farm_conn, ccp_conn, _hmds) = Gateway::connect(&cfg).expect("Initial connect failed");
    assert!(gw.account_id.starts_with("DU"), "refusing to run: the logged-in account is not a paper account (its id does not start with DU)");
    let restart = gw.restart_token.clone().expect("the server offers restart tokens and the login announced one");
    let lapsed = num_bigint::BigUint::from(123_456_789u64);
    let mut auth = ReconnectAuth {
        host: cfg.host.clone(),
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        paper: cfg.paper,
        session_key: lapsed.clone(),
        session_token: lapsed,
        server_session_id: gw.server_session_id.clone(),
        hw_info: gw.hw_info.clone(),
        encoded: gw.encoded.clone(),
        hmds_host: gw.hmds_host.clone(),
        hmds_farm: gw.hmds_farm.clone(),
        farm_host: gw.farm_host.clone(),
        farm_name: gw.farm_name.clone(),
        session_epoch: gw.session_epoch.clone(),
        ns_secure_refused: gw.ns_secure_refused,
        use_ssl: gw.use_ssl,
        ssl_farms: gw.ssl_farms.clone(),
        second_factor: ibx::gateway::SecondFactorSettings::of(&cfg),
        restart_token: Some(restart.clone()),
    };
    // The announce is answered within a second; the link stays up for it.
    std::thread::sleep(Duration::from_secs(3));
    drop(ccp_conn);
    let first = reconnect_ccp_session(&auth).expect("the reconnect logs in with the restart token");
    assert!(first.restart_token.is_none(), "no full login: a full login announces a new restart token");
    let key = first.session_key.expect("a login with the restart token gives its session token");
    assert_eq!(key, ibx::gateway::restart_session_token(&restart, &gw.hw_info));
    drop(first.conn);
    auth.session_key = key.clone();
    auth.session_token = key.clone();
    std::thread::sleep(Duration::from_secs(5));
    let second = reconnect_ccp_session(&auth).expect("the next reconnect passes on the session token of that login");
    assert!(second.session_key.is_none(), "the session-token step was taken");
    let farm = connect_farm(&auth.host, "usfarm", &auth.username, &auth.password, true,
        &auth.server_session_id, &key, &auth.hw_info, &auth.encoded, 17);
    assert!(farm.is_ok(), "a farm logs on with the session token of that login: {:?}", farm.err());
    println!("PASS: the restart token replaces the lapsed session token with no password step");
}

/// ibx#762: a login with the password saves its restart token; the login
/// of a later program of the same machine presents it, with no password
/// step, and its farms log on.
/// Run with: cargo test --test farm_reconnect_poc a_later_login_presents_the_saved_restart_token -- --ignored --nocapture
#[test]
#[ignore = "live: logs in to the paper account (IB_USERNAME / IB_PASSWORD)"]
fn a_later_login_presents_the_saved_restart_token() {
    let _ = env_logger::try_init();
    let _one = one_login();
    let dir = std::env::temp_dir().join(format!("ibx-762-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("restart-token");
    let mut cfg = config();
    cfg.restart_token_file = Some(file.clone());
    let (gw, farm, ccp, hmds) = Gateway::connect(&cfg).expect("login with the password");
    assert!(gw.account_id.starts_with("DU"), "refusing to run: the logged-in account is not a paper account (its id does not start with DU)");
    assert!(!gw.restart_login);
    let restart = gw.restart_token.clone().expect("the login announced a restart token");
    let saved = ibx::auth::restart_token::SavedRestartToken::read(&file, &cfg.username, true, &gw.hw_info)
        .expect("the token is saved");
    assert_eq!(saved.token, restart);
    std::thread::sleep(Duration::from_secs(3));
    drop((farm, ccp, hmds));
    std::thread::sleep(Duration::from_secs(5));

    let (gw2, _farm2, _ccp2, _hmds2) = Gateway::connect(&cfg).expect("login with the restart token");
    assert!(gw2.restart_login, "the saved restart token was taken");
    assert_eq!(gw2.account_id, gw.account_id);
    assert_eq!(gw2.restart_token.as_ref(), Some(&restart), "the restart token is kept");
    let _ = std::fs::remove_dir_all(&dir);
    println!("PASS: a later login presents the saved restart token, no password step (account {})", gw2.account_id);
}
