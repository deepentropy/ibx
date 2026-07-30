//! Verify issue #176: live login through a typed second-factor code rather than
//! a mobile push. Serves both factors — the 8-character IBKey
//! Challenge/Response code, and an authenticator app's code — reading whichever
//! the account uses from stdin.
//!
//! Run:
//!   $env:RUST_LOG="info"; cargo run --release --example ex176_live_2fa_code_provider
//!
//! Requires `IB_LIVE_USERNAME` / `IB_LIVE_PASSWORD` in `.env` (auto-loaded).
//! Type the code at the prompt. Authenticator codes expire in ~30s, so read a
//! fresh one when asked rather than ahead of time.
//!
//! Note: one wrong code is terminal — the server skips AUTH_FINISH and tears
//! the socket down (ib-agent#149). There is no retry loop.

use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::time::Instant;

use ibx::auth::session::{self, CodeProvider, IbKeyChallenge, SecondFactor};
use ibx::gateway::{Gateway, GatewayConfig};
use zeroize::Zeroizing;

/// Minimal `.env` loader (KEY=VALUE per line, `#` comments, no quoting tricks).
fn load_dotenv() {
    if let Ok(text) = fs::read_to_string(".env") {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                let v = v.trim().trim_matches('"').trim_matches('\'');
                if env::var_os(k).is_none() {
                    // SAFETY: single-threaded at startup, before any thread spawns.
                    unsafe { env::set_var(k, v); }
                }
            }
        }
    }
}

/// Prompt the operator for whichever code this account's factor calls for.
fn read_code_from_stdin(challenge: IbKeyChallenge) -> io::Result<String> {
    println!();
    let (banner, prompt, want) = match challenge.factor {
        SecondFactor::IbKeyChallengeResponse => (
            "== IBKey Challenge/Response ==",
            "Enter the 8-character code shown in the IBKey app: ",
            Some(8),
        ),
        // Length is not fixed across authenticator configurations, so take
        // what the operator types rather than rejecting a valid code.
        SecondFactor::AuthenticatorCode => (
            "== Authenticator code ==",
            "Enter the current code from your authenticator app: ",
            None,
        ),
    };
    println!("{}", banner);
    if !challenge.display_id.is_empty() {
        println!("   display_id : {}", challenge.display_id);
    }
    if !challenge.avth_url.is_empty() {
        println!("   avth_url   : {}", challenge.avth_url);
    }
    print!("{}", prompt);
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let code = line.trim().to_string();
    if code.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no code entered"));
    }
    if let Some(n) = want {
        if code.len() != n {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("expected {} characters, got {}", n, code.len()),
            ));
        }
    }
    Ok(code)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    load_dotenv();

    let username = env::var("IB_LIVE_USERNAME")
        .map_err(|_| "IB_LIVE_USERNAME not set (.env or shell)")?;
    let password = env::var("IB_LIVE_PASSWORD")
        .map_err(|_| "IB_LIVE_PASSWORD not set (.env or shell)")?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());

    let provider: CodeProvider = Arc::new(read_code_from_stdin);

    let config = GatewayConfig {
        username,
        password: Zeroizing::new(password),
        host: host.clone(),
        paper: false,
        accept_invalid_certs: false,
        ib_key_timeout_secs: session::IB_KEY_DEFAULT_TIMEOUT_SECS,
        ib_key_token_sub_type: session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
        code_provider: Some(provider),
    };

    println!("== Connecting LIVE ({}). Waiting for the second-factor challenge...", host);
    let t0 = Instant::now();
    let (gw, _farm, _ccp, _hmds) = Gateway::connect(&config)?;
    let elapsed = t0.elapsed().as_secs_f64();
    println!();
    println!("PASS — issue #176 verified: second-factor login succeeded in {:.1}s (account_id={})",
        elapsed, gw.account_id);
    Ok(())
}
