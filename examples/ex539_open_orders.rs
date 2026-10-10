//! ibx#539 probe. Read-only: login, the open orders of the account, their
//! count, disconnect. No order is sent.
//!
//! Run after a benchmark that places orders: it must print 0.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::{Contract, Order, OrderState};
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct Open {
    orders: Vec<String>,
    end: bool,
}

impl Wrapper for Open {
    fn open_order(&mut self, order_id: i64, contract: &Contract, order: &Order, state: &OrderState) {
        self.orders.push(format!(
            "order {order_id}: {} {} {} {} @ {} {} ({})",
            order.action, order.total_quantity, contract.symbol, order.order_type, order.lmt_price, order.tif, state.status,
        ));
    }
    fn open_order_end(&mut self) {
        self.end = true;
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
    })?;
    println!("logged in paper (account {})", client.account_id);

    // The orders of the account come with the logon: left to arrive.
    let mut open = Open::default();
    let settle = Instant::now() + Duration::from_secs(3);
    while Instant::now() < settle {
        client.process_msgs(&mut open);
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut open = Open::default();
    client.req_all_open_orders(&mut open);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !open.end && Instant::now() < deadline {
        client.process_msgs(&mut open);
        std::thread::sleep(Duration::from_millis(20));
    }
    client.disconnect();
    for line in &open.orders {
        println!("{line}");
    }
    println!("{} open orders (list ended: {})", open.orders.len(), open.end);
    Ok(())
}
