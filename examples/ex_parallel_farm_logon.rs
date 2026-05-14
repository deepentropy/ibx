//! Validate whether the server accepts two `connect_farm` calls in parallel.
//!
//! The `Gateway::connect` flow currently serializes farm logons (see
//! `gateway.rs:1482`), citing a possible "competing login" rejection on live.
//! Parallelizing would roughly halve the farm-logon phase. This example
//! measures sequential vs parallel timings and reports whether both farms
//! authenticate successfully.
//!
//! Run paper (default):
//!   $env:RUST_LOG="info"; cargo run --release --example ex_parallel_farm_logon
//!
//! Run live (requires IB_LIVE_USERNAME/PASSWORD; expect a 2FA push):
//!   $env:RUST_LOG="info"; cargo run --release --example ex_parallel_farm_logon -- --live
//!
//! Assumes a US-routed account (hardcodes `usfarm` / `ushmds`, matching
//! `tests/farm_reconnect_poc.rs`). For EU accounts the route names differ.

use std::env;
use std::fs;
use std::thread;
use std::time::Instant;

use ibx::gateway::{connect_farm, Gateway, GatewayConfig};

fn load_dotenv() {
    if let Ok(text) = fs::read_to_string(".env") {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                let v = v.trim().trim_matches('"').trim_matches('\'');
                if env::var_os(k).is_none() {
                    unsafe { env::set_var(k, v); }
                }
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    load_dotenv();

    let live = env::args().any(|a| a == "--live");
    let (user_var, pass_var) = if live {
        ("IB_LIVE_USERNAME", "IB_LIVE_PASSWORD")
    } else {
        ("IB_USERNAME", "IB_PASSWORD")
    };
    let username = env::var(user_var).map_err(|_| format!("{} not set", user_var))?;
    let password = env::var(pass_var).map_err(|_| format!("{} not set", pass_var))?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());

    let cfg = GatewayConfig {
        username: username.clone(),
        password: zeroize::Zeroizing::new(password.clone()),
        host: host.clone(),
        paper: !live,
        accept_invalid_certs: false,
        ib_key_timeout_secs: ibx::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
        ib_key_token_sub_type: ibx::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
        code_provider: None,
    };

    println!("== Initial Gateway::connect ({}, host={})", if live { "LIVE" } else { "PAPER" }, host);
    let t0 = Instant::now();
    let (gw, farm_conn, _ccp_conn, hmds) = Gateway::connect(&cfg)?;
    let initial_ms = t0.elapsed().as_millis();
    println!("   initial connect: {} ms (account={})", initial_ms, gw.account_id);

    let session_token = gw.session_token.clone();
    let server_session_id = gw.server_session_id.clone();
    let hw_info = gw.hw_info.clone();
    let encoded = gw.encoded.clone();

    drop(farm_conn);
    drop(hmds);
    println!("   dropped both farm connections");

    // -------- Experiment A: SERIAL reconnect (baseline) --------
    println!("\n== A: SERIAL reconnect (baseline)");
    let t_a = Instant::now();
    let t_a1 = Instant::now();
    let trading_a = connect_farm(
        &host, "usfarm", &cfg.username, &cfg.password, cfg.paper,
        &server_session_id, &session_token, &hw_info, &encoded, 18,
    );
    let trading_a_ms = t_a1.elapsed().as_millis();
    let t_a2 = Instant::now();
    let mktdata_a = connect_farm(
        &host, "ushmds", &cfg.username, &cfg.password, cfg.paper,
        &server_session_id, &session_token, &hw_info, &encoded, 17,
    );
    let mktdata_a_ms = t_a2.elapsed().as_millis();
    let serial_total_ms = t_a.elapsed().as_millis();
    println!("   trading (usfarm): {} ms — {}", trading_a_ms, summarize(&trading_a));
    println!("   mktdata (ushmds): {} ms — {}", mktdata_a_ms, summarize(&mktdata_a));
    println!("   serial total:     {} ms", serial_total_ms);
    let serial_ok = trading_a.is_ok() && mktdata_a.is_ok();
    drop(trading_a);
    drop(mktdata_a);

    // Brief pause to make sure the server treats the next pair as fresh.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // -------- Experiment B: PARALLEL reconnect (the candidate optimization) --------
    println!("\n== B: PARALLEL reconnect");
    let host_b = host.clone();
    let user_b = cfg.username.clone();
    let pass_b: zeroize::Zeroizing<String> = zeroize::Zeroizing::new((*cfg.password).clone());
    let paper_b = cfg.paper;
    let ssid_b = server_session_id.clone();
    let token_b = session_token.clone();
    let hw_b = hw_info.clone();
    let enc_b = encoded.clone();

    let t_b = Instant::now();
    let t_handle = {
        let host = host_b.clone();
        let user = user_b.clone();
        let pass = zeroize::Zeroizing::new((*pass_b).clone());
        let ssid = ssid_b.clone();
        let token = token_b.clone();
        let hw = hw_b.clone();
        let enc = enc_b.clone();
        thread::spawn(move || {
            let t = Instant::now();
            let r = connect_farm(&host, "usfarm", &user, &pass, paper_b,
                &ssid, &token, &hw, &enc, 18);
            (t.elapsed().as_millis(), r)
        })
    };
    let m_handle = {
        let host = host_b.clone();
        let user = user_b.clone();
        let pass = zeroize::Zeroizing::new((*pass_b).clone());
        let ssid = ssid_b.clone();
        let token = token_b.clone();
        let hw = hw_b.clone();
        let enc = enc_b.clone();
        thread::spawn(move || {
            let t = Instant::now();
            let r = connect_farm(&host, "ushmds", &user, &pass, paper_b,
                &ssid, &token, &hw, &enc, 17);
            (t.elapsed().as_millis(), r)
        })
    };

    let (trading_b_ms, trading_b) = t_handle.join().expect("trading thread panicked");
    let (mktdata_b_ms, mktdata_b) = m_handle.join().expect("mktdata thread panicked");
    let parallel_total_ms = t_b.elapsed().as_millis();
    println!("   trading (usfarm): {} ms — {}", trading_b_ms, summarize(&trading_b));
    println!("   mktdata (ushmds): {} ms — {}", mktdata_b_ms, summarize(&mktdata_b));
    println!("   parallel total:   {} ms", parallel_total_ms);
    let parallel_ok = trading_b.is_ok() && mktdata_b.is_ok();

    // -------- Verdict --------
    println!("\n== Verdict");
    println!("   serial   farms succeeded : {}", serial_ok);
    println!("   parallel farms succeeded : {}", parallel_ok);
    if parallel_ok {
        let saved = serial_total_ms.saturating_sub(parallel_total_ms);
        println!("   parallel saved {} ms vs serial ({} → {})",
            saved, serial_total_ms, parallel_total_ms);
        println!("\nPASS — server accepts parallel farm logon. Safe to parallelize gateway.rs:1485.");
    } else {
        println!("\nFAIL — at least one parallel farm logon failed. Keep serialization.");
    }
    Ok(())
}

fn summarize<T, E: std::fmt::Display>(r: &Result<T, E>) -> String {
    match r {
        Ok(_) => "OK".into(),
        Err(e) => format!("ERR: {}", e),
    }
}

