//! Diagnostic tool for issue #109: reqAccountSummary returns DailyPnL=0
//! Connects to IB paper, subscribes to account updates + PnL, logs everything.

use ibx::api::{EClient, EClientConfig, TickAttrib, Wrapper};
use std::env;
use std::time::Instant;

struct PnlDiag {
    start: Instant,
}

impl Wrapper for PnlDiag {
    fn error(&mut self, req_id: i64, error_code: i64, error_string: &str, _: &str) {
        let elapsed = self.start.elapsed().as_millis();
        eprintln!("[{elapsed}ms] ERROR req_id={req_id} code={error_code}: {error_string}");
    }

    fn account_summary(
        &mut self,
        req_id: i64,
        account: &str,
        tag: &str,
        value: &str,
        currency: &str,
    ) {
        let elapsed = self.start.elapsed().as_millis();
        println!(
            "[{elapsed}ms] ACCOUNT_SUMMARY req_id={req_id} account={account} tag={tag} value={value} currency={currency}"
        );
    }

    fn account_summary_end(&mut self, req_id: i64) {
        let elapsed = self.start.elapsed().as_millis();
        println!("[{elapsed}ms] ACCOUNT_SUMMARY_END req_id={req_id}");
    }

    fn pnl(&mut self, req_id: i64, daily_pnl: f64, unrealized_pnl: f64, realized_pnl: f64) {
        let elapsed = self.start.elapsed().as_millis();
        println!(
            "[{elapsed}ms] PNL req_id={req_id} daily={daily_pnl} unrealized={unrealized_pnl} realized={realized_pnl}"
        );
    }

    fn pnl_single(
        &mut self,
        req_id: i64,
        pos: f64,
        daily_pnl: f64,
        unrealized_pnl: f64,
        realized_pnl: f64,
        value: f64,
    ) {
        let elapsed = self.start.elapsed().as_millis();
        println!(
            "[{elapsed}ms] PNL_SINGLE req_id={req_id} pos={pos} daily={daily_pnl} unrealized={unrealized_pnl} realized={realized_pnl} value={value}"
        );
    }

    fn update_account_value(&mut self, key: &str, value: &str, currency: &str, _account: &str) {
        let elapsed = self.start.elapsed().as_millis();
        if key.contains("PnL")
            || key.contains("pnl")
            || key.contains("Pnl")
            || key == "NetLiquidation"
            || key == "UnrealizedPnL"
            || key == "DailyPnL"
            || key == "RealizedPnL"
        {
            println!("[{elapsed}ms] ACCOUNT_VALUE key={key} value={value} currency={currency}");
        }
    }

    fn update_account_time(&mut self, timestamp: &str) {
        let elapsed = self.start.elapsed().as_millis();
        println!("[{elapsed}ms] ACCOUNT_TIME timestamp={timestamp}");
    }

    fn account_download_end(&mut self, account: &str) {
        let elapsed = self.start.elapsed().as_millis();
        println!("[{elapsed}ms] ACCOUNT_DOWNLOAD_END account={account}");
    }

    // Ignore ticks
    fn tick_price(&mut self, _: i64, _: i32, _: f64, _: &TickAttrib) {}
    fn tick_size(&mut self, _: i64, _: i32, _: f64) {}
}

fn main() {
    let _log = ibx::logging::init(&ibx::logging::LogConfig::from_env());

    let username = env::var("IB_USERNAME").expect("Set IB_USERNAME");
    let password = env::var("IB_PASSWORD").expect("Set IB_PASSWORD");

    println!("Connecting to IB paper...");
    let client = EClient::connect(&EClientConfig {
        username,
        password,
        host: "cdc1.ibllc.com".into(),
        paper: true,
        core_id: None,
    })
    .unwrap_or_else(|e| {
        eprintln!("Connection failed: {e}");
        std::process::exit(1);
    });
    println!("Connected!");

    // Subscribe to account updates (populates AccountState from UT/UM messages)
    client.req_account_updates(true, "");
    println!("Subscribed to account updates");

    // Subscribe to PnL
    client.req_pnl(100, "", "");
    println!("Subscribed to PnL (req_id=100)");

    // Request account summary with PnL tags
    client.req_account_summary(
        200,
        "All",
        "NetLiquidation,BuyingPower,DailyPnL,UnrealizedPnL,RealizedPnL",
    );
    println!("Requested account summary (req_id=200)");

    let mut wrapper = PnlDiag {
        start: Instant::now(),
    };
    let deadline = Instant::now() + std::time::Duration::from_secs(15);

    while Instant::now() < deadline {
        client.process_msgs(&mut wrapper);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Request summary again after 15s of account updates
    println!("\n=== Requesting account summary again after 15s ===");
    client.req_account_summary(
        300,
        "All",
        "NetLiquidation,BuyingPower,DailyPnL,UnrealizedPnL,RealizedPnL",
    );

    let deadline2 = Instant::now() + std::time::Duration::from_secs(5);
    while Instant::now() < deadline2 {
        client.process_msgs(&mut wrapper);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    println!("\nDiagnostic complete.");
}
