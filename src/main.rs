use std::env;

use ibx::{Client, ClientConfig, Event};
use ibx::types::*;

fn main() {
    env_logger::init();
    println!("ibx v{}", env!("CARGO_PKG_VERSION"));

    let username = env::var("IB_USERNAME").unwrap_or_else(|_| {
        eprintln!("Set IB_USERNAME and IB_PASSWORD environment variables");
        std::process::exit(1);
    });
    let password = env::var("IB_PASSWORD").unwrap_or_else(|_| {
        eprintln!("Set IB_PASSWORD environment variable");
        std::process::exit(1);
    });
    let paper = env::var("IB_PAPER").unwrap_or_else(|_| "true".to_string()) == "true";
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());

    let client = Client::connect(&ClientConfig {
        username,
        password,
        host,
        paper,
        core_id: None,
    }).unwrap_or_else(|e| {
        eprintln!("Connection failed: {}", e);
        std::process::exit(1);
    });

    println!("Connected! Account: {}", client.account_id);

    let spy = client.subscribe(756733, "SPY");
    println!("Subscribed to SPY (instrument={})", spy);

    while let Ok(event) = client.recv() {
        match event {
            Event::Tick(instrument) if instrument == spy => {
                let q = client.quote(spy);
                println!(
                    "SPY bid={:.2} ask={:.2} last={:.2}",
                    q.bid as f64 / PRICE_SCALE as f64,
                    q.ask as f64 / PRICE_SCALE as f64,
                    q.last as f64 / PRICE_SCALE as f64,
                );
            }
            Event::Fill(fill) => {
                println!("Fill: order={} qty={} price={:.2}",
                    fill.order_id, fill.qty,
                    fill.price as f64 / PRICE_SCALE as f64);
            }
            Event::OrderUpdate(update) => {
                println!("Order {}: {:?}", update.order_id, update.status);
            }
            Event::Disconnected => {
                println!("Disconnected.");
                break;
            }
            _ => {}
        }
    }
}
