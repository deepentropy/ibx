//! Subscribe to every trade and quote change on a contract, and keep the frames
//! exactly as the venue sends them.
//!
//! A decoder checked only against synthetic frames proves nothing about the
//! ones that arrive, so this captures real ones.
//!
//! Reads only. It places nothing.
//!
//!     IB_USERNAME=… IB_PASSWORD=… IBX_CAPTURE_WIRE=1 cargo run --features dev-tools --bin capture_ticks

#[path = "support/window.rs"]
mod window;
use window::live_window_is_open;

use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::Contract;

/// Contracts that trade outside the American session, so this can be run
/// before the New York open. Both are entitled on this account.
fn subjects() -> Vec<(&'static str, &'static str, Contract)> {
    if std::env::var("IBX_CRYPTO_ONLY").is_ok() {
        return vec![("a crypto, quotes", "BidAsk", Contract {
            symbol: "BTC".to_string(),
            sec_type: "CRYPTO".to_string(),
            exchange: "PAXOS".to_string(),
            currency: "USD".to_string(),
            ..Default::default()
        })];
    }
    if let Ok(kind) = std::env::var("IBX_LAST_PROBE") {
        let kind: &'static str = if kind == "Last" { "Last" } else { "AllLast" };
        return vec![(kind, kind, Contract {
            symbol: "MES".to_string(), sec_type: "FUT".to_string(),
            exchange: "CME".to_string(), currency: "USD".to_string(),
            last_trade_date_or_contract_month: "202612".to_string(),
            ..Default::default()
        })];
    }
    if std::env::var("IBX_TRADES_ONLY").is_ok() {
        return vec![("a busy listing", "AllLast", Contract {
            symbol: "SPY".to_string(),
            sec_type: "STK".to_string(),
            exchange: "SMART".to_string(),
            currency: "USD".to_string(),
            ..Default::default()
        })];
    }
    vec![
        // A currency pair has quotes and no trades, so asking for trades is
        // asking for something that does not exist — and the venue says so.
        ("a currency pair", "BidAsk", Contract {
            symbol: "EUR".to_string(),
            sec_type: "CASH".to_string(),
            exchange: "IDEALPRO".to_string(),
            currency: "USD".to_string(),
            ..Default::default()
        }),
        ("a crypto, quotes", "BidAsk", Contract {
            symbol: "BTC".to_string(),
            sec_type: "CRYPTO".to_string(),
            exchange: "PAXOS".to_string(),
            currency: "USD".to_string(),
            ..Default::default()
        }),
        ("a crypto, trades", "AllLast", Contract {
            symbol: "BTC".to_string(),
            sec_type: "CRYPTO".to_string(),
            exchange: "PAXOS".to_string(),
            currency: "USD".to_string(),
            ..Default::default()
        }),
        // A busy American listing during its own session, where the flags a
        // trade carries — reported away from the exchange, or through a limit
        // — actually occur.
        ("a busy listing", "AllLast", Contract {
            symbol: "SPY".to_string(),
            sec_type: "STK".to_string(),
            exchange: "SMART".to_string(),
            currency: "USD".to_string(),
            ..Default::default()
        }),
    ]
}

/// Record frames printed per subject. A dump at this size states so.
const DUMP_LIMIT: usize = 200;

fn main() {
    let _ = ibx::logging::try_init_from_env("error");

    // This binary logs in with the paper credentials, which nothing else is
    // using. Only a run against the live account has to wait for a window.
    let against_live = std::env::var("IB_PAPER").as_deref() == Ok("0");
    if against_live && !live_window_is_open() {
        eprintln!(
            "the live account may be in use by another program — a live run \
             waits for the premarket window or for after the close. The paper \
             account is reachable now."
        );
        std::process::exit(3);
    }
    // Safety: set before anything reads it, and this binary is single-threaded
    // until the engine starts.
    unsafe { std::env::set_var("IBX_CAPTURE_WIRE", "1") };

    let username = std::env::var("IB_USERNAME").unwrap_or_default();
    let password = std::env::var("IB_PASSWORD").unwrap_or_default();
    if username.trim().is_empty() || password.trim().is_empty() {
        eprintln!("IB_USERNAME/IB_PASSWORD unset. This reads from real servers.");
        std::process::exit(2);
    }

    let config = EClientConfig {
        username,
        password,
        host: std::env::var("IB_HOST").unwrap_or_default(),
        // The run this tool was asked for, not one it decided on: the gate
        // above waits for a window when the live account was named, and
        // connecting to paper anyway made that wait mean nothing.
        paper: !against_live,
        ..Default::default()
    };

    let client = match EClient::connect(&config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("could not open a session: {e}");
            std::process::exit(1);
        }
    };
    println!("session open");

    for (n, (what, kind, contract)) in subjects().into_iter().enumerate() {
        // Frames already accumulated before this subject subscribed.
        // Everything past this mark belongs to it.
        let seen_before = client
            .unread_wire()
            .iter()
            .filter(|(kind, _)| *kind == "hmds-msg")
            .count();
        let req = n as i64 + 1;
        // Resolve it first: a subscription wants the contract's own id, and the
        // id is also what says the venue knows the contract at all.
        let resolved = match client.qualify_contract(&contract) {
            Ok(c) => c,
            Err(e) => {
                println!("  {what:<20} could not be resolved: {e}");
                continue;
            }
        };
        println!("  {what:<20} conId={}", resolved.con_id);

        // No market-data subscription. The venue states the increments when it
        // takes the tick subscription on, so asking for market data as well was
        // only ever a way of learning something already on the way.
        if let Err(e) = client.req_tick_by_tick_data(req, &resolved, kind, 0, false) {
            println!("  {what:<20} the subscription was refused: {e}");
            continue;
        }

        // Wait for frames. A quiet market and a feed that will never speak look
        // the same from here, so the wait is bounded and reported either way.
        let deadline = Instant::now() + Duration::from_secs(25);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }

        // What actually reached a caller, which is the only thing that counts.
        // Only this contract's. Draining everything and printing it reports
        // one contract's market under another's name, which is the very thing
        // being checked for.
        // Which request each record says it arrived under. A contract can
        // carry several streams, and the contract alone does not say which.
        let all_quotes = client.shared_state().market.drain_tbt_quotes();
        let all_trades = client.shared_state().market.drain_tbt_trades();
        let mine = client.instrument_of(resolved.con_id);
        let quotes: Vec<_> = all_quotes
            .iter()
            .filter(|q| Some(q.instrument) == mine)
            .collect();
        let trades: Vec<_> = all_trades
            .iter()
            .filter(|t| Some(t.instrument) == mine)
            .collect();
        let requests: std::collections::BTreeSet<i64> = quotes
            .iter()
            .map(|q| q.req_id)
            .chain(trades.iter().map(|t| t.req_id))
            .collect();
        println!(
            "        instrument={mine:?}  asked as req {req}  records say {requests:?}",
        );
        println!(
            "  {what:<20} delivered: {} quote(s), {} trade(s)",
            quotes.len(),
            trades.len()
        );
        for q in quotes.iter().take(3) {
            println!(
                "        bid {:.5} x {:.0}   ask {:.5} x {:.0}",
                q.bid as f64 / 1e8,
                q.bid_size as f64 / ibx::types::QTY_SCALE as f64,
                q.ask as f64 / 1e8,
                q.ask_size as f64 / ibx::types::QTY_SCALE as f64
            );
        }
        println!(
            "        of {} trade(s): {} marked unreported, {} past the limit",
            trades.len(),
            trades.iter().filter(|t| t.unreported).count(),
            trades.iter().filter(|t| t.past_limit).count(),
        );
        for t in trades.iter().take(3) {
            println!(
                "        traded {:.2} x {:.4} on {:<6} past_limit={} unreported={}",
                t.price as f64 / 1e8,
                t.size as f64 / ibx::types::QTY_SCALE as f64,
                t.exchange,
                t.past_limit,
                t.unreported,
            );
        }

        // `unread_wire` accumulates for the life of the session and never
        // clears, so a subject's own frames are the ones past the mark taken
        // before it subscribed. Read cumulatively, every subject reports the
        // first subscription's traffic.
        let all: Vec<String> = client
            .unread_wire()
            .into_iter()
            .filter(|(kind, _)| *kind == "hmds-msg")
            .map(|(_, hex)| hex)
            .collect();
        let frames: Vec<String> = all[seen_before.min(all.len())..].to_vec();

        if frames.is_empty() {
            println!("  {what:<20} nothing arrived in twenty seconds");
        } else {
            println!("  {what:<20} {} frame(s):", frames.len());
            // Group by message type, so the shape of the traffic is visible
            // rather than a wall of bytes.
            let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
            for hex in &frames {
                let bytes: Vec<u8> = (0..hex.len() / 2)
                    .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0))
                    .collect();
                let text = String::from_utf8_lossy(&bytes);
                let kind = text
                    .split('\u{1}')
                    .find_map(|f| f.strip_prefix("35=").map(|v| v.to_string()))
                    .unwrap_or_else(|| "?".to_string());
                *kinds.entry(kind).or_default() += 1;
            }
            for (kind, n) in &kinds {
                println!("        35={kind}: {n}");
            }
            // A few whole frames, so the layout can be checked against the
            // decoder rather than against a guess.
            let records: Vec<&String> = frames.iter().filter(|h| {
                let bytes: Vec<u8> = (0..h.len() / 2)
                    .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap_or(0))
                    .collect();
                String::from_utf8_lossy(&bytes).contains("35=E")
            }).collect::<Vec<_>>();
            let shown = records.len().min(DUMP_LIMIT);
            if records.len() > shown {
                println!("        showing {shown} of {} record frame(s)", records.len());
            }
            for hex in records.iter().take(shown) {
                println!("        E {hex}");
            }
            // The acknowledgement, which is where the venue says what number
            // it has given this subscription.
            for hex in &frames {
                let bytes: Vec<u8> = (0..hex.len() / 2)
                    .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0))
                    .collect();
                let text = String::from_utf8_lossy(&bytes);
                if text.contains("35=W") && !text.contains("<error>") {
                    // Whole, on one line, with the separators made visible.
                    let shown: String = text
                        .chars()
                        .map(|c| if c == '\u{1}' { '|' } else if c == '\n' { ' ' } else { c })
                        .collect();
                    println!("        ack: {shown}");
                }
            }
            // The venue states refusals in plain words. Show them.
            for hex in &frames {
                let bytes: Vec<u8> = (0..hex.len() / 2)
                    .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0))
                    .collect();
                let text = String::from_utf8_lossy(&bytes);
                if let Some(at) = text.find("<error>") {
                    let rest = &text[at + 7..];
                    if let Some(end) = rest.find("</error>") {
                        println!("        the venue says: {}", &rest[..end]);
                    }
                }
            }
        }
    }

    client.disconnect();
}
