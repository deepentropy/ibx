//! ibx#574 probe. Read-only: login, one scanner parameters request; the
//! text of the `scannerParameters` callback and of the event channel are
//! compared. No order is sent.
//!
//! Prints the length of each text, whether they are equal, and the time
//! of each from the request.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;
use ibx::bridge::Event;

#[derive(Default)]
struct Params(Option<String>);

impl Wrapper for Params {
    fn scanner_parameters(&mut self, xml: &str) {
        self.0 = Some(xml.to_string());
    }
}

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
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        return Err("not a paper account: nothing sent".into());
    }
    let start = Instant::now();
    client.req_scanner_parameters()?;
    let mut callback = Params::default();
    let (mut from_event, mut at_callback, mut at_event) = (None, None, None);
    let deadline = Instant::now() + Duration::from_secs(30);
    while (callback.0.is_none() || from_event.is_none()) && Instant::now() < deadline {
        client.process_msgs(&mut callback);
        if callback.0.is_some() && at_callback.is_none() {
            at_callback = Some(start.elapsed().as_millis());
        }
        if let Ok(Event::ScannerParameters(xml)) = events.recv_timeout(Duration::from_millis(5)) {
            at_event = Some(start.elapsed().as_millis());
            from_event = Some(xml);
        }
    }
    let len = |t: &Option<String>| t.as_ref().map_or(0, String::len);
    println!("callback: {} characters at {:?} ms", len(&callback.0), at_callback);
    println!("event:    {} characters at {:?} ms", len(&from_event), at_event);
    println!("equal: {}", callback.0.is_some() && callback.0 == from_event);
    client.disconnect();
    Ok(())
}
