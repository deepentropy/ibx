//! Issue #150 — verify FIX tag 6321 (PRIV_LAB_MISC_URLS) is parsed at logon.
//!
//! Connects, prints the populated misc_urls map and the region_dam lookup
//! used by webapp REST consumers.

use std::env;

use ibx::api::client::{EClient, EClientConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let username = env::var("IB_USERNAME")?;
    let password = env::var("IB_PASSWORD")?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());

    let client = EClient::connect(&EClientConfig {
        username, password, host, paper: true, core_id: None,
    })?;

    println!("ccp_session_id: {:?}", client.ccp_session_id());
    for key in [
        "region_dam", "region_webserver", "nossl", "s3store",
        "scope", "cookbook", "fileservice",
    ] {
        println!("misc_url({:?}) = {:?}", key, client.misc_url(key));
    }

    client.disconnect();
    Ok(())
}
