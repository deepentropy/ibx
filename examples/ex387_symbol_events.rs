//! ibx#387 probe. Read-only: login, two matching-symbols requests, their
//! answers read from the event channel, disconnect.
//!
//! Prints one line per `Event::SymbolSamples`: a pattern with matches and
//! one without (an empty list).
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::bridge::Event;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (client, events) = EClient::connect_with_events(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
    }, 1024)?;
    println!("logged in paper (account {})", client.account_id);

    let mut answered = 0;
    for (req_id, pattern) in [(1, "AAPL"), (2, "ZZQXJWK")] {
        client.req_matching_symbols(req_id, pattern)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Ok(event) = events.recv_deadline(deadline) {
            if let Event::SymbolSamples { req_id, matches } = event {
                println!("symbol_samples event req {} count {}", req_id, matches.len());
                for m in matches.iter().take(3) {
                    println!("  {} {} {} {} conId={}", m.symbol, m.sec_type, m.primary_exchange, m.currency, m.con_id);
                }
                answered += 1;
                break;
            }
        }
    }
    client.disconnect();
    println!("{answered} of 2 requests answered on the event channel");
    Ok(())
}
