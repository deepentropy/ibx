//! ibx#610 / ibx#612 probe. Paper account only. Prints the answers to a
//! configuration request and an update at the session level ibx answers
//! (225): the installation's configuration, and the update's answer
//! (below level 219 and 221 they were refused with 503).
//!
//! Env: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::config::{ConfigRequest, UpdateConfigRequest};
use ibx::api::wrapper::Wrapper;

struct W { start: Instant }
impl Wrapper for W {
    fn config_response(&mut self, response: &ibx::api::config::ConfigResponse) {
        println!("{:6.3}s config_response {:?}", self.start.elapsed().as_secs_f64(), response.req_id);
    }
    fn update_config_response(&mut self, response: &ibx::api::config::UpdateConfigResponse) {
        println!("{:6.3}s update_config_response {:?}", self.start.elapsed().as_secs_f64(), response);
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("{:6.3}s error {} {} {}", self.start.elapsed().as_secs_f64(), id, code, msg);
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
        client_id: 0,
        copied_waits: false,
        code_provider: None,
        restart_token_file: None,
    }).unwrap();
    if !client.account_id.starts_with("DU") { client.disconnect(); panic!("not a paper account"); }
    let mut w = W { start: Instant::now() };
    pump(&client, &mut w, 3);
    println!("=== session level {}", client.server_version());
    w.start = Instant::now();
    client.update_config(&UpdateConfigRequest { req_id: Some(7401), ..Default::default() });
    client.update_config(&UpdateConfigRequest::default());
    client.req_config(&ConfigRequest { req_id: Some(7301) });
    client.req_config(&ConfigRequest::default());
    pump(&client, &mut w, 3);
    client.disconnect();
}
