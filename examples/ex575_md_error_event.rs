//! ibx#575 probe. Read-only: login, one market data request the server
//! refuses on the paper account, its refusal read from the event channel
//! WITHOUT processing the client's messages, then with them. No order.
//!
//! Prints the `MarketDataError` event the engine gives by instrument, then
//! the `Error` event the client gives with the request id once
//! `process_msgs` runs.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::Contract;
use ibx::api::wrapper::Wrapper;
use ibx::bridge::Event;

struct Quiet;
impl Wrapper for Quiet {}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (client, events) = EClient::connect_with_events(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
        client_id: 0,
        copied_waits: false,
        code_provider: None,
        restart_token_file: None,
    }, 4096)?;
    println!("logged in paper (account {})", client.account_id);

    // No live data for this contract on the paper account.
    let toyota = Contract {
        con_id: 13905804, symbol: "7203".into(), sec_type: "STK".into(), exchange: "SMART".into(),
        currency: "JPY".into(), ..Default::default()
    };
    let start = Instant::now();
    client.req_mkt_data(9001, &toyota, "", false, false)?;

    // Phase 1: the channel alone.
    let mut by_instrument = 0;
    let deadline = Instant::now() + Duration::from_secs(15);
    while by_instrument == 0 {
        match events.recv_deadline(deadline) {
            Ok(Event::MarketDataError { instrument, code, message }) => {
                println!("+{} ms, channel only: instrument {instrument} code {code}: {message}", start.elapsed().as_millis());
                by_instrument += 1;
            }
            // Link notices have id -1; an error with the request id is not expected yet.
            Ok(Event::Error { req_id, code, .. }) if req_id > 0 => println!("unexpected before process_msgs: Error req {req_id} code {code}"),
            Ok(_) => {}
            Err(_) => break,
        }
    }

    // Phase 2: the client processes its messages and gives the request id.
    let mut by_request = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while by_request == 0 && Instant::now() < deadline {
        client.process_msgs(&mut Quiet);
        while let Ok(event) = events.try_recv() {
            if let Event::Error { req_id: 9001, code, message } = event {
                println!("after process_msgs: request 9001 code {code}: {message}");
                by_request += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    client.disconnect();
    println!("{by_instrument} event by instrument without process_msgs, {by_request} event by request id with it");
    Ok(())
}
