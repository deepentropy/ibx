//! ibx#577 probe. Paper account only: login, the requests whose answers
//! were covered by unit tests only, their events read from the event
//! channel, disconnect. Needs no open market.
//!
//! It places ONE limit order far from the market (BUY 1 AAPL at 1.00) and
//! cancels it, for the open order and completed order events. It stops
//! unless the account id starts with DU.
//!
//! Prints one line per kind seen, then the kinds that did not come.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::collections::BTreeMap;
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::{Contract, Order, ScannerSubscription};
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

    let stock = |con_id: i64, symbol: &str, currency: &str| Contract {
        con_id, symbol: symbol.into(), sec_type: "STK".into(), exchange: "SMART".into(),
        currency: currency.into(), ..Default::default()
    };
    let aapl = stock(265598, "AAPL", "USD");
    // An AAPL call of December 2026 (seen on 11/10/2026).
    let option_con_id = env::var("BENCH_OPTION_CON_ID").ok().and_then(|v| v.parse().ok()).unwrap_or(692806985);
    let option = Contract { con_id: option_con_id, exchange: "SMART".into(), ..Default::default() };

    client.req_scanner_subscription(1, &ScannerSubscription {
        number_of_rows: 5, instrument: "STK".into(), location_code: "STK.US.MAJOR".into(),
        scan_code: "TOP_PERC_GAIN".into(), ..Default::default()
    }, &[], &[])?;
    client.req_fundamental_data(2, &aapl, "ReportSnapshot")?;
    client.req_historical_news(3, 265598, "BRFG+BRFUPDN+DJ-N", "", "", 5)?;
    // Alone, so that its definition lookup is not behind the lookups of a
    // whole option month: BENCH_OPTION_CON_ID names another option.
    client.calculate_option_price(4, &option, 0.30, 250.0, &[])?;
    // No live data for this contract on the paper account.
    client.req_mkt_data(5, &stock(13905804, "7203", "JPY"), "", false, false)?;

    let order_id = client.next_order_id();
    client.place_order(order_id, &aapl, &Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 1.0,
        tif: "GTC".into(), transmit: true, ..Default::default()
    })?;
    println!("placed order {order_id}: BUY 1 AAPL LMT 1.00 GTC");

    let expected = [
        "ScannerData", "FundamentalData", "HistoricalNews", "NewsArticle", "OptionComputation",
        "MarketDataError", "OpenOrder", "CompletedOrder",
    ];
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut errors: Vec<String> = Vec::new();
    let mut cancelled = false;
    let mut article_asked = false;
    let deadline = Instant::now() + Duration::from_secs(40);
    while !expected.iter().all(|k| seen.contains_key(*k)) && Instant::now() < deadline {
        client.process_msgs(&mut Quiet);
        let event = match events.recv_timeout(Duration::from_millis(50)) {
            Ok(event) => event,
            Err(_) => continue,
        };
        let (kind, detail) = match event {
            Event::ScannerData { req_id, result } => ("ScannerData", format!("req {req_id}: {} rows", result.entries.len())),
            Event::FundamentalData { req_id, data } => ("FundamentalData", format!("req {req_id}: {} characters", data.len())),
            Event::HistoricalNews { req_id, headlines, has_more } => {
                if let (Some(first), false) = (headlines.first(), article_asked) {
                    article_asked = true;
                    client.req_news_article(6, &first.provider_code, &first.article_id)?;
                }
                ("HistoricalNews", format!("req {req_id}: {} headlines, more {has_more}", headlines.len()))
            }
            Event::NewsArticle { req_id, article_type, text } => ("NewsArticle", format!("req {req_id}: type {article_type}, {} characters", text.len())),
            Event::OptionComputation(a) => ("OptionComputation", format!("req {}: price {:.4}, delta {:.4}", a.req_id, a.opt_price, a.delta)),
            Event::MarketDataError { instrument, code, .. } => ("MarketDataError", format!("instrument {instrument}: code {code}")),
            Event::OpenOrder { order_id: id, info } => {
                // Its cancel, once the order is known to the server.
                if id == order_id && !cancelled {
                    cancelled = true;
                    client.cancel_order(order_id, "")?;
                }
                ("OpenOrder", format!("order {id}: {} {} {}", info.order.action, info.contract.symbol, info.order_state.status))
            }
            Event::CompletedOrder(o) => ("CompletedOrder", format!("order {}: {:?}", o.order_id, o.status)),
            Event::Error { req_id, code, message } if req_id > 0 => {
                errors.push(format!("req {req_id} code {code}: {}", message.chars().take(110).collect::<String>()));
                continue;
            }
            _ => continue,
        };
        seen.entry(kind.to_string()).or_insert(detail);
    }
    // Nothing is left working, whatever was seen.
    if !cancelled {
        client.cancel_order(order_id, "")?;
    }
    let settle = Instant::now() + Duration::from_secs(3);
    while Instant::now() < settle {
        client.process_msgs(&mut Quiet);
        std::thread::sleep(Duration::from_millis(20));
    }
    client.disconnect();

    for (kind, detail) in &seen {
        println!("{kind}: {detail}");
    }
    for e in &errors {
        println!("error event: {e}");
    }
    let missing: Vec<_> = expected.iter().filter(|k| !seen.contains_key(**k)).collect();
    println!("{} of {} kinds received on the event channel; missing: {missing:?}", expected.len() - missing.len(), expected.len());
    Ok(())
}
