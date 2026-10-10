//! ibx#553 probe. Read-only: login, one scanner subscription on a fresh
//! session, then a scanner parameters request once the parameters are
//! known. No order is sent.
//!
//! Prints the time from the subscription to its first rows (it includes
//! the delay of the parameters request and two server answers), and the
//! time of the parameters request made after, answered from what is kept.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::ScannerSubscription;
use ibx::api::wrapper::Wrapper;
use ibx::bridge::Event;

struct Quiet;
impl Wrapper for Quiet {}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (client, events) = EClient::connect_with_events(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
    }, 4096)?;
    println!("logged in paper (account {})", client.account_id);

    let wait = |what: &str, since: Instant, is: &dyn Fn(&Event) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            client.process_msgs(&mut Quiet);
            match events.recv_timeout(Duration::from_millis(5)) {
                Ok(Event::Error { req_id, code, message }) if req_id > 0 => println!("error req {req_id} code {code}: {message}"),
                Ok(event) if is(&event) => return println!("{what}: {} ms", since.elapsed().as_millis()),
                _ if Instant::now() > deadline => return println!("{what}: nothing in 30 s"),
                _ => {}
            }
        }
    };

    let scan = ScannerSubscription {
        number_of_rows: 5, instrument: "STK".into(), location_code: "STK.US.MAJOR".into(),
        scan_code: "TOP_PERC_GAIN".into(), ..Default::default()
    };
    let start = Instant::now();
    client.req_scanner_subscription(1, &scan, &[], &[])?;
    wait("first subscription to its first rows", start, &|e| matches!(e, Event::ScannerData { req_id: 1, .. }));

    let start = Instant::now();
    client.req_scanner_parameters()?;
    wait("parameters request once they are known", start, &|e| matches!(e, Event::ScannerParameters(_)));

    client.cancel_scanner_subscription(1)?;
    std::thread::sleep(Duration::from_millis(500));
    client.disconnect();
    Ok(())
}
