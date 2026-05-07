//! ibx#151 — verify reconnect_ccp falls back to SRP when the server demands it.
//!
//! Phase 1: reconnect with the valid cached session_token (SOFT_TOKEN path,
//!          regression check).
//! Phase 2: clobber session_token to provoke an AUTH_START that signals SRP
//!          required, then verify the new fallback completes the handshake
//!          using the cached username/password.

use std::env;
use std::time::Instant;

use ibx::gateway::{reconnect_ccp, Gateway, GatewayConfig, ReconnectAuth};
use num_bigint::BigUint;
use zeroize::Zeroizing;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let username = env::var("IB_USERNAME")?;
    let password = env::var("IB_PASSWORD")?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());

    let cfg = GatewayConfig {
        username: username.clone(),
        password: Zeroizing::new(password.clone()),
        host: host.clone(),
        paper: true,
        accept_invalid_certs: false,
        ib_key_timeout_secs: ibx::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
        ib_key_token_sub_type: ibx::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
    };

    println!("== Connecting to paper {} ...", host);
    let t0 = Instant::now();
    let (gw, _farm, ccp, _hmds) = Gateway::connect(&cfg)?;
    println!(
        "== Connected in {:.1}s account={} session_id={}",
        t0.elapsed().as_secs_f64(),
        gw.account_id,
        gw.server_session_id,
    );

    let mut auth = ReconnectAuth {
        host: host.clone(),
        username: username.clone(),
        password: Zeroizing::new(password.clone()),
        paper: true,
        session_key: gw.session_token.clone(),
        session_token: gw.session_token.clone(),
        server_session_id: gw.server_session_id.clone(),
        hw_info: gw.hw_info.clone(),
        encoded: gw.encoded.clone(),
    };

    drop(ccp);

    println!("\n== Phase 1: SOFT_TOKEN reconnect with valid session_token");
    let t1 = Instant::now();
    let ccp1 = reconnect_ccp(&auth)?;
    println!(
        "== Phase 1 OK ({}ms) seq={}",
        t1.elapsed().as_millis(),
        ccp1.seq,
    );
    drop(ccp1);

    println!("\n== Phase 2: clobber session_token, expect SRP fallback");
    auth.session_token = BigUint::from(0xDEADBEEFu32);
    let t2 = Instant::now();
    match reconnect_ccp(&auth) {
        Ok(ccp2) => {
            println!(
                "== Phase 2 OK ({}ms) seq={} — SRP fallback path verified",
                t2.elapsed().as_millis(),
                ccp2.seq,
            );
            Ok(())
        }
        Err(e) => {
            eprintln!(
                "== Phase 2 FAIL ({}ms): {}",
                t2.elapsed().as_millis(),
                e,
            );
            eprintln!(
                "   (server may have rejected the bogus token before AUTH_START — \
                 the SRP path triggers only when the server returns auth_mode=0)",
            );
            Err(e.into())
        }
    }
}
