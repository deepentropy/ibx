//! ibx#480 probe. Read-only: login, reqSoftDollarTiers, disconnect.
//!
//! Prints the soft dollar tiers ibx returns (from logon tag 6522; none when
//! the logon has no tiers, as on paper).
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
//! Live (read-only, may ask for a second-factor approval): IBX_LIVE=1 with
//! IB_LIVE_USERNAME, IB_LIVE_PASSWORD.
use std::env;

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;

struct W;
impl Wrapper for W {
    fn soft_dollar_tiers(&mut self, req_id: i64, tiers: &[ibx::types::SoftDollarTier]) {
        println!("soft_dollar_tiers req {} count {}", req_id, tiers.len());
        for t in tiers {
            println!("  name={:?} val={:?} display={:?}", t.name, t.val, t.display_name);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let live = env::var("IBX_LIVE").is_ok_and(|v| v == "1");
    let (user, pass) = if live { ("IB_LIVE_USERNAME", "IB_LIVE_PASSWORD") } else { ("IB_USERNAME", "IB_PASSWORD") };
    let client = EClient::connect(&EClientConfig {
        username: env::var(user)?, password: env::var(pass)?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: !live, core_id: None,
    })?;
    println!("logged in {} (account {})", if live { "LIVE" } else { "paper" }, client.account_id);
    client.req_soft_dollar_tiers(1, &mut W);
    client.disconnect();
    Ok(())
}
