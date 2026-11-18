//! ibx#602 probe through the Rust client at the session level ibx answers
//! (225), paper account only: contract details (C), historical bars (H),
//! frozen market data, a snapshot then a short stream (M), account summary
//! (S), positions (P), the configuration (G), a far limit order placed and
//! cancelled (O), an order with no time in force (T: refused with 10052 at
//! 225). Prints the callbacks; nothing can fill and no order is left.
//!
//! Paper: IB_USERNAME, IB_PASSWORD. CASES: the letters to run (default
//! CHMSPGOT).
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::config::{ConfigRequest, ConfigResponse};
use ibx::api::types::{BarData, ContractDetails, OrderState, TickAttrib};
use ibx::api::wrapper::Wrapper;

struct Log {
    t0: Instant,
}

impl Log {
    fn line(&self, text: String) {
        println!("   +{:6} ms  {text}", self.t0.elapsed().as_millis());
    }
}

impl Wrapper for Log {
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) {
        if !matches!(code, 2104 | 2106 | 2107 | 2108 | 2119 | 2158) {
            self.line(format!("error {req_id} {code} {text}"));
        }
    }
    fn contract_details(&mut self, req_id: i64, d: &ContractDetails) {
        self.line(format!("contractDetails {req_id} {} {} {}", d.contract.con_id, d.contract.symbol, d.min_tick));
    }
    fn contract_details_end(&mut self, req_id: i64) { self.line(format!("contractDetailsEnd {req_id}")); }
    fn historical_data(&mut self, req_id: i64, b: &BarData) {
        self.line(format!("historicalData {req_id} {} {} {} {} {} {}", b.date, b.open, b.high, b.low, b.close, b.volume));
    }
    fn historical_data_end(&mut self, req_id: i64, start: &str, end: &str) {
        self.line(format!("historicalDataEnd {req_id} {start} {end}"));
    }
    fn market_data_type(&mut self, req_id: i64, t: i32) { self.line(format!("marketDataType {req_id} {t}")); }
    fn tick_req_params(&mut self, req_id: i64, min_tick: f64, bbo: &str, permissions: i64) {
        self.line(format!("tickReqParams {req_id} {min_tick} {bbo} {permissions}"));
    }
    fn tick_price(&mut self, req_id: i64, t: i32, price: f64, a: &TickAttrib) {
        self.line(format!("tickPrice {req_id} {t} {price} auto={}", a.can_auto_execute));
    }
    fn tick_size(&mut self, req_id: i64, t: i32, size: f64) { self.line(format!("tickSize {req_id} {t} {size}")); }
    fn tick_string(&mut self, req_id: i64, t: i32, v: &str) { self.line(format!("tickString {req_id} {t} {v}")); }
    fn tick_generic(&mut self, req_id: i64, t: i32, v: f64) { self.line(format!("tickGeneric {req_id} {t} {v}")); }
    fn tick_snapshot_end(&mut self, req_id: i64) { self.line(format!("tickSnapshotEnd {req_id}")); }
    fn account_summary(&mut self, req_id: i64, _account: &str, tag: &str, _value: &str, currency: &str) {
        self.line(format!("accountSummary {req_id} {tag} {currency}"));
    }
    fn account_summary_end(&mut self, req_id: i64) { self.line(format!("accountSummaryEnd {req_id}")); }
    fn position(&mut self, _account: &str, c: &Contract, pos: f64, _avg_cost: f64) {
        self.line(format!("position {} {} {pos}", c.symbol, c.sec_type));
    }
    fn position_end(&mut self) { self.line("positionEnd".into()); }
    fn config_response(&mut self, r: &ConfigResponse) {
        self.line(format!("configResponse {:?} messages={} api={} orders={}", r.req_id, r.messages.len(), r.api.is_some(), r.orders.is_some()));
    }
    fn open_order(&mut self, order_id: i64, _: &Contract, o: &Order, s: &OrderState) {
        self.line(format!("openOrder {order_id} {} {} {}", o.order_type, o.tif, s.status));
    }
    fn order_status(&mut self, order_id: i64, status: &str, _: f64, remaining: f64, _: f64, _: i64, _: i64,
        _: f64, _: i64, _: &str, _: f64) {
        self.line(format!("orderStatus {order_id} {status} remaining={remaining}"));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
        client_id: 198,
        copied_waits: false,
        code_provider: None,
        restart_token_file: None,
    })?;
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        return Err("not a paper account: nothing sent".into());
    }
    let pump = |client: &EClient, w: &mut Log, secs: f64| {
        let until = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < until {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let mut w = Log { t0: Instant::now() };
    pump(&client, &mut w, 3.0);
    println!("session level {}", client.server_version());
    let aapl = Contract { symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    let lmt = |tif: &str| Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 1.0, tif: tif.into(), ..Default::default()
    };
    w.t0 = Instant::now();
    for case in env::var("CASES").unwrap_or_else(|_| "CHMSPGOT".into()).chars() {
        w.line(format!("--- case {case}"));
        match case {
            'C' => { client.req_contract_details(9001, &aapl)?; pump(&client, &mut w, 3.0); }
            'H' => {
                client.req_historical_data(9002, &aapl, "", "2 D", "1 hour", "TRADES", true, 1, false)?;
                pump(&client, &mut w, 5.0);
            }
            'M' => {
                client.req_market_data_type(2);
                client.req_mkt_data(9003, &aapl, "", true, false)?;
                pump(&client, &mut w, 13.0);
                client.req_mkt_data(9004, &aapl, "", false, false)?;
                pump(&client, &mut w, 4.0);
                client.cancel_mkt_data(9004)?;
                client.req_market_data_type(1);
                pump(&client, &mut w, 0.5);
            }
            'S' => {
                client.req_account_summary(9005, "All", "NetLiquidation,TotalCashValue,BuyingPower,$LEDGER:USD");
                pump(&client, &mut w, 4.0);
                client.cancel_account_summary(9005);
            }
            'P' => { client.req_positions(&mut w); pump(&client, &mut w, 3.0); client.cancel_positions(); }
            'G' => { client.req_config(&ConfigRequest { req_id: Some(7301) }); pump(&client, &mut w, 4.0); }
            'O' => {
                let id = client.next_order_id();
                client.place_order(id, &aapl, &lmt("DAY"))?;
                pump(&client, &mut w, 4.0);
                client.cancel_order(id, "")?;
                pump(&client, &mut w, 3.0);
            }
            'T' => {
                let id = client.next_order_id();
                let _ = client.place_order(id, &aapl, &lmt(""));
                pump(&client, &mut w, 4.0);
                let _ = client.cancel_order(id, "");
                pump(&client, &mut w, 2.0);
            }
            _ => {}
        }
    }
    pump(&client, &mut w, 2.0);
    client.disconnect();
    Ok(())
}
