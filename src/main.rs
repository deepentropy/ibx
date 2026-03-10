use std::env;

use ibx::engine::context::{Context, Strategy};
use ibx::gateway::{Gateway, GatewayConfig};
use ibx::types::*;

/// Example strategy: buys when spread exceeds threshold, places take-profit on fill.
struct SimpleMomentum {
    threshold: Price,
    order_size: u32,
}

impl Strategy for SimpleMomentum {
    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
        let spread = ctx.spread(instrument);
        if spread > self.threshold && ctx.position(instrument) == 0 {
            ctx.submit_limit(instrument, Side::Buy, self.order_size, ctx.bid(instrument));
        }
    }

    fn on_fill(&mut self, fill: &Fill, ctx: &mut Context) {
        let tp = fill.price + self.threshold * 2;
        ctx.submit_limit(fill.instrument, Side::Sell, fill.qty as u32, tp);
    }
}

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

    let config = GatewayConfig {
        username,
        password,
        host,
        paper,
    };

    println!("Connecting to IB (paper={})...", config.paper);
    let (gw, farm_conn, ccp_conn, hmds_conn) = match Gateway::connect(&config) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("Connection failed: {}", e);
            std::process::exit(1);
        }
    };
    println!("Connected! Account: {}", gw.account_id);

    let strategy = SimpleMomentum {
        threshold: 5 * PRICE_SCALE,
        order_size: 100,
    };

    let (mut hot_loop, _control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    // Register instruments (example: AAPL conId)
    let _aapl = hot_loop.context_mut().register_instrument(265598);

    println!("Engine ready. Starting hot loop...");
    hot_loop.run();
    println!("Engine stopped.");
}
