//! ibx#578 probe, for regular hours. Paper account only: login, the
//! answers that need an open market read from the event channel, disconnect.
//!
//! Buys 1 AAPL at the market and sells it back (two commission reports),
//! asks 5 second real-time bars and a historical request kept up to date.
//! It stops unless the account id starts with DU.
//!
//! Prints one line per kind seen, then the kinds that did not come.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::collections::BTreeMap;
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::{Contract, Order};
use ibx::api::wrapper::Wrapper;
use ibx::bridge::Event;

struct Quiet;
impl Wrapper for Quiet {}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _log = ibx::logging::init(&ibx::logging::LogConfig::from_env());
    let (client, events) = EClient::connect_with_events(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
        client_id: 0,
        copied_waits: false,
        code_provider: None,
        restart_token_file: None,
    }, 8192)?;
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        return Err("not a paper account: nothing sent".into());
    }
    println!("logged in paper (account {})", client.account_id);

    let aapl = Contract {
        con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(),
        currency: "USD".into(), ..Default::default()
    };
    let market = |action: &str| Order {
        action: action.into(), total_quantity: 1.0, order_type: "MKT".into(), tif: "DAY".into(),
        transmit: true, ..Default::default()
    };

    client.req_real_time_bars(1, &aapl, 5, "TRADES", false)?;
    client.req_historical_data(2, &aapl, "", "1800 S", "1 min", "TRADES", false, 1, true)?;
    let buy_id = client.next_order_id();
    let sell_id = buy_id + 1;
    client.place_order(buy_id, &aapl, &market("BUY"))?;
    println!("placed order {buy_id}: BUY 1 AAPL MKT");

    let expected = ["RealTimeBar", "HistoricalUpdate", "Fill", "CommissionReport"];
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    let mut errors: Vec<String> = Vec::new();
    let mut sold = false;
    let mut sell_filled = false;
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline
        && !(expected.iter().all(|k| seen.contains_key(*k)) && sell_filled && counts.get("RealTimeBar") >= Some(&2)) {
        client.process_msgs(&mut Quiet);
        let event = match events.recv_timeout(Duration::from_millis(50)) {
            Ok(event) => event,
            Err(_) => continue,
        };
        let (kind, detail) = match event {
            Event::RealTimeBar { req_id, bar } => ("RealTimeBar", format!("req {req_id}: close {:.2} volume {}", bar.close, bar.volume)),
            Event::HistoricalUpdate { req_id, bar } => ("HistoricalUpdate", format!("req {req_id}: {} close {:.2}", bar.time, bar.close)),
            Event::Fill(fill) => {
                if fill.order_id == buy_id && !sold {
                    sold = true;
                    client.place_order(sell_id, &aapl, &market("SELL"))?;
                    println!("placed order {sell_id}: SELL 1 AAPL MKT");
                }
                sell_filled |= fill.order_id == sell_id;
                ("Fill", format!("order {}", fill.order_id))
            }
            Event::CommissionReport(r) => ("CommissionReport", format!("{} {} {}", r.exec_id, r.commission_and_fees, r.currency)),
            Event::Error { req_id, code, message } if req_id > 0 => {
                errors.push(format!("req {req_id} code {code}: {}", message.chars().take(110).collect::<String>()));
                continue;
            }
            _ => continue,
        };
        *counts.entry(kind).or_insert(0) += 1;
        seen.entry(kind.to_string()).or_insert(detail);
    }
    client.cancel_real_time_bars(1)?;
    client.cancel_historical_data(2)?;
    // Nothing is left working nor held, whatever was seen.
    if !sold {
        client.cancel_order(buy_id, "")?;
    }
    let settle = Instant::now() + Duration::from_secs(3);
    while Instant::now() < settle {
        client.process_msgs(&mut Quiet);
        std::thread::sleep(Duration::from_millis(20));
    }
    client.disconnect();

    for (kind, detail) in &seen {
        println!("{kind}: {detail} ({} in all)", counts[kind.as_str()]);
    }
    for e in &errors {
        println!("error event: {e}");
    }
    if sold && !sell_filled {
        println!("WARNING: the sell {sell_id} was not seen filled: check the AAPL position");
    }
    let missing: Vec<_> = expected.iter().filter(|k| !seen.contains_key(**k)).collect();
    println!("{} of {} kinds received on the event channel; missing: {missing:?}", expected.len() - missing.len(), expected.len());
    Ok(())
}
