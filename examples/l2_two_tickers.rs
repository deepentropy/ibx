//! Subscribe to L2 market depth for two tickers and print the order book.
//!
//! Usage:
//!   IB_USERNAME=user IB_PASSWORD=pass cargo run --example l2_two_tickers
//!
//! Optional env vars:
//!   IB_HOST       — gateway host (default: cdc1.ibllc.com)
//!   DURATION_SECS — how long to collect data (default: 15)

use std::collections::HashMap;
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig, Contract};
use ibx::api::wrapper::Wrapper;

// ── Book entry ──

#[derive(Debug, Clone)]
struct BookLevel {
    price: f64,
    size: f64,
    market_maker: String,
}

// ── Per-ticker book ──

struct TickerBook {
    symbol: String,
    bids: HashMap<i32, BookLevel>,  // position → level
    asks: HashMap<i32, BookLevel>,
    update_count: u64,
}

impl TickerBook {
    fn new(symbol: &str) -> Self {
        Self {
            symbol: symbol.into(),
            bids: HashMap::new(),
            asks: HashMap::new(),
            update_count: 0,
        }
    }

    fn apply(&mut self, position: i32, market_maker: &str, operation: i32, side: i32, price: f64, size: f64) {
        self.update_count += 1;
        let book = if side == 1 { &mut self.bids } else { &mut self.asks };
        match operation {
            0 | 1 => { book.insert(position, BookLevel { price, size, market_maker: market_maker.into() }); }
            2 => { book.remove(&position); }
            _ => {}
        }
    }

    fn print_summary(&self) {
        let mut bids: Vec<_> = self.bids.values().collect();
        let mut asks: Vec<_> = self.asks.values().collect();
        bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap());
        asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap());

        println!("\n── {} ── ({} updates)", self.symbol, self.update_count);
        println!("  Top 5 Asks:");
        for level in asks.iter().take(5).rev() {
            println!("    {:>10.2} x {:<10.0}  {}", level.price, level.size, level.market_maker);
        }
        println!("  ─────────────────────────────");
        println!("  Top 5 Bids:");
        for level in bids.iter().take(5) {
            println!("    {:>10.2} x {:<10.0}  {}", level.price, level.size, level.market_maker);
        }
    }
}

// ── Wrapper that collects depth for 2 tickers ──

struct DepthWrapper {
    books: HashMap<i64, TickerBook>,  // req_id → book
    errors: Vec<(i64, i64, String)>,
}

impl DepthWrapper {
    fn new(tickers: &[(i64, &str)]) -> Self {
        let mut books = HashMap::new();
        for &(req_id, symbol) in tickers {
            books.insert(req_id, TickerBook::new(symbol));
        }
        Self { books, errors: Vec::new() }
    }

    fn total_updates(&self) -> u64 {
        self.books.values().map(|b| b.update_count).sum()
    }
}

impl Wrapper for DepthWrapper {
    fn error(&mut self, req_id: i64, error_code: i64, error_string: &str, _: &str) {
        eprintln!("  error req_id={} code={} msg={}", req_id, error_code, error_string);
        self.errors.push((req_id, error_code, error_string.into()));
    }

    fn update_mkt_depth(
        &mut self, req_id: i64, position: i32, operation: i32,
        side: i32, price: f64, size: f64,
    ) {
        if let Some(book) = self.books.get_mut(&req_id) {
            book.apply(position, "", operation, side, price, size);
        }
    }

    fn update_mkt_depth_l2(
        &mut self, req_id: i64, position: i32, market_maker: &str,
        operation: i32, side: i32, price: f64, size: f64, _is_smart_depth: bool,
    ) {
        if let Some(book) = self.books.get_mut(&req_id) {
            book.apply(position, market_maker, operation, side, price, size);
        }
    }
}

// ── Contracts ──

fn aapl() -> Contract {
    Contract {
        con_id: 265598,
        symbol: "AAPL".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    }
}

fn msft() -> Contract {
    Contract {
        con_id: 272093,
        symbol: "MSFT".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    }
}

fn main() {
    let username = env::var("IB_USERNAME").expect("IB_USERNAME required");
    let password = env::var("IB_PASSWORD").expect("IB_PASSWORD required");
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into());
    let duration_secs: u64 = env::var("DURATION_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);

    println!("Connecting to {}...", host);
    let client = EClient::connect(&EClientConfig {
        username,
        password,
        host,
        paper: true,
        core_id: None,
        code_provider: None,
    }).expect("Failed to connect");
    println!("Connected.");

    // Subscribe to L2 depth for both tickers (5 rows, SmartDepth)
    let num_rows = 5;
    let is_smart = true;

    let aapl_id: i64 = 1;
    let msft_id: i64 = 2;

    println!("Subscribing AAPL (req_id={}) depth rows={} smart={}", aapl_id, num_rows, is_smart);
    client.req_mkt_depth(aapl_id, &aapl(), num_rows, is_smart)
        .expect("Failed to subscribe AAPL depth");

    println!("Subscribing MSFT (req_id={}) depth rows={} smart={}", msft_id, num_rows, is_smart);
    client.req_mkt_depth(msft_id, &msft(), num_rows, is_smart)
        .expect("Failed to subscribe MSFT depth");

    // Poll for updates
    let mut wrapper = DepthWrapper::new(&[(aapl_id, "AAPL"), (msft_id, "MSFT")]);
    let start = Instant::now();
    let timeout = Duration::from_secs(duration_secs);

    println!("Collecting depth data for {}s...", duration_secs);
    while start.elapsed() < timeout {
        client.process_msgs(&mut wrapper);
        std::thread::sleep(Duration::from_millis(50));
    }

    // Print results
    for book in wrapper.books.values() {
        book.print_summary();
    }

    let total = wrapper.total_updates();
    println!("\nTotal depth updates: {}", total);

    // Cancel subscriptions
    let _ = client.cancel_mkt_depth(aapl_id);
    let _ = client.cancel_mkt_depth(msft_id);

    // Validate
    for (req_id, book) in &wrapper.books {
        assert!(
            book.update_count > 0,
            "No depth updates received for {} (req_id={})", book.symbol, req_id
        );
        assert!(
            !book.bids.is_empty() || !book.asks.is_empty(),
            "Empty book for {} (req_id={})", book.symbol, req_id
        );
        println!("✓ {} — {} updates, {} bid levels, {} ask levels",
            book.symbol, book.update_count, book.bids.len(), book.asks.len());
    }
    assert!(total > 0, "Expected depth updates but got none");
    println!("\nAll validations passed.");
}
