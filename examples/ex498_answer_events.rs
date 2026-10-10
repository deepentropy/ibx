//! ibx#498 probe. Read-only: login, a set of requests, their answers read
//! from the event channel, disconnect. No order is sent.
//!
//! Prints one line per answer kind seen on the channel, then the kinds
//! that were expected and did not come.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::collections::BTreeMap;
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
    }, 4096)?;
    println!("logged in paper (account {})", client.account_id);

    let aapl = Contract {
        con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(),
        currency: "USD".into(), ..Default::default()
    };
    client.req_contract_details(1, &aapl)?;
    client.req_sec_def_opt_params(2, "AAPL", "", "STK", 265598)?;
    client.req_histogram_data(3, &aapl, true, "3 days")?;
    client.req_historical_data(4, &aapl, "", "5 D", "1 day", "SCHEDULE", true, 1, false)?;
    client.req_historical_ticks(5, &aapl, "20261009 10:00:00 US/Eastern", "", 10, "TRADES", true, false, &[])?;
    client.req_scanner_parameters()?;
    client.req_account_summary(6, "All", "NetLiquidation");
    // Answered by the client itself: its error has no queue.
    client.req_market_rule(999_999, &mut Quiet);

    let expected = [
        "OptionChains", "MarketRules", "HistogramData", "HistoricalSchedule", "HistoricalTicks",
        "ScannerParameters", "AccountSummary", "Error 322",
    ];
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !expected.iter().all(|k| seen.contains_key(*k)) {
        // The account summary and the errors of the client come with its messages.
        client.process_msgs(&mut Quiet);
        let event = match events.recv_timeout(Duration::from_millis(50)) {
            Ok(event) => event,
            Err(_) if Instant::now() < deadline => continue,
            Err(_) => break,
        };
        let (kind, detail) = match event {
            Event::OptionChains { req_id, chains } => ("OptionChains".to_string(), format!("req {req_id}: {} rows", chains.len())),
            Event::MarketRules(rules) => ("MarketRules".into(), format!("{} rules", rules.len())),
            Event::HistogramData { req_id, entries } => ("HistogramData".into(), format!("req {req_id}: {} prices", entries.len())),
            Event::HistoricalSchedule { req_id, data } => ("HistoricalSchedule".into(), format!("req {req_id}: {} sessions", data.sessions.len())),
            Event::HistoricalTicks { req_id, done, what_to_show, .. } => ("HistoricalTicks".into(), format!("req {req_id}: {what_to_show}, done {done}")),
            Event::ScannerParameters(xml) => ("ScannerParameters".into(), format!("{} bytes", xml.len())),
            Event::AccountSummary(s) => ("AccountSummary".into(), format!("{} rows, end {}", s.rows.len(), s.end)),
            Event::Error { req_id, code, message } => (format!("Error {code}"), format!("req {req_id}: {message}")),
            _ => continue,
        };
        seen.entry(kind).or_insert(detail);
    }
    client.disconnect();
    for (kind, detail) in &seen {
        println!("{kind}: {detail}");
    }
    let missing: Vec<_> = expected.iter().filter(|k| !seen.contains_key(**k)).collect();
    println!("{} of {} answer kinds received on the event channel; missing: {missing:?}", expected.len() - missing.len(), expected.len());
    Ok(())
}
