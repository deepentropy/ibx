//! ibx#424 probe. Paper account only. Read-only: no orders.
//!
//! The four display group requests: the list of groups, a subscription
//! (`none` at once), refused subscriptions (321), updates (321 for bad
//! input, 473 for a conId that is not a contract, nothing for a valid
//! one), and unsubscribe.
//!
//! Env: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;

struct W { start: Instant }
impl Wrapper for W {
    fn display_group_list(&mut self, req_id: i64, groups: &str) {
        println!("{:6.1}s display_group_list {} {}", self.start.elapsed().as_secs_f64(), req_id, groups);
    }
    fn display_group_updated(&mut self, req_id: i64, contract_info: &str) {
        println!("{:6.1}s display_group_updated {} {}", self.start.elapsed().as_secs_f64(), req_id, contract_info);
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("{:6.1}s error {} {} {}", self.start.elapsed().as_secs_f64(), id, code, msg);
    }
}

fn pump(client: &EClient, w: &mut W, secs: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        client.process_msgs(w);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn main() {
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME").unwrap(), password: env::var("IB_PASSWORD").unwrap(),
        host: "cdc1.ibllc.com".into(), paper: true, core_id: None,
    }).unwrap();
    if !client.account_id.starts_with("DU") { client.disconnect(); panic!("not a paper account"); }
    let mut w = W { start: Instant::now() };
    pump(&client, &mut w, 3);
    println!("=== query, subscribe");
    client.query_display_groups(1, &mut w);
    client.subscribe_to_group_events(2, 1, &mut w);
    client.subscribe_to_group_events(3, 9, &mut w);
    client.subscribe_to_group_events(2, 1, &mut w);
    println!("=== update: refusals");
    client.update_display_group(8, "265598@SMART");
    client.update_display_group(2, "abc@SMART");
    client.update_display_group(2, "265598@SMART|action=Foo");
    pump(&client, &mut w, 2);
    println!("=== update: conId that is not a contract (473)");
    client.update_display_group(2, "999999999@SMART");
    pump(&client, &mut w, 4);
    println!("=== update: valid conIds (nothing)");
    client.update_display_group(2, "8314@SMART");
    client.update_display_group(2, "none");
    pump(&client, &mut w, 4);
    println!("=== the same conId again: seen before, no lookup (nothing)");
    client.update_display_group(2, "8314@SMART");
    pump(&client, &mut w, 3);
    println!("=== unsubscribe");
    client.unsubscribe_from_group_events(9);
    client.unsubscribe_from_group_events(2);
    client.unsubscribe_from_group_events(2);
    pump(&client, &mut w, 2);
    client.disconnect();
}
