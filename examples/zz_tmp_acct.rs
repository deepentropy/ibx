//! Temporary: log account frames (ibx#475). Paper only. Not committed.
use std::env;
use std::time::{Duration, Instant};
use ibx::api::client::{EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;

struct L;
impl log::Log for L {
    fn enabled(&self, _: &log::Metadata) -> bool { true }
    fn log(&self, r: &log::Record) {
        let m = r.args().to_string();
        if m.starts_with("WIRE") {
            for t in ["|35=UT|", "|35=UM|", "|35=RL|", "|35=EB|", "|35=UP|", "|35=AR|", "|6040=74|", "|6040=77|", "|6040=75|"] {
                if m.contains(t) {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
                    eprintln!("{} {}", now, m);
                    break;
                }
            }
        }
    }
    fn flush(&self) {}
}
struct W;
impl Wrapper for W {}

fn main() {
    static LOGGER: L = L;
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let secs: u64 = env::var("PROBE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(240);
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME").unwrap(), password: env::var("IB_PASSWORD").unwrap(),
        host: "cdc1.ibllc.com".into(), paper: true, core_id: None,
    }).unwrap();
    if !client.account_id.starts_with("DU") { client.disconnect(); panic!("not paper"); }
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        client.process_msgs(&mut W);
        std::thread::sleep(Duration::from_millis(10));
    }
    client.disconnect();
}
