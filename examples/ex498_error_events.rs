//! ibx#498 probe. Read-only: login, two requests that fail, their errors
//! read from the event channel, disconnect.
//!
//! Prints one line per `Event::Error`: a contract lookup the server
//! refuses, and a matching-symbols pattern refused before it is sent.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::Contract;
use ibx::bridge::Event;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (client, events) = EClient::connect_with_events(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
    }, 1024)?;
    println!("logged in paper (account {})", client.account_id);

    let unknown = Contract {
        symbol: "ZZQXJWK".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(),
        ..Default::default()
    };
    client.req_contract_details(1, &unknown)?;
    client.req_matching_symbols(2, "")?;

    let mut answered = 0;
    let deadline = Instant::now() + Duration::from_secs(15);
    while answered < 2 {
        match events.recv_deadline(deadline) {
            Ok(Event::Error { req_id, code, message }) if req_id > 0 => {
                println!("error event req {req_id} code {code}: {message}");
                answered += 1;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    client.disconnect();
    println!("{answered} of 2 errors received on the event channel");
    Ok(())
}
