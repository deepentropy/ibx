//! ibapi-compatible Wrapper trait — Rust equivalent of C++ `EWrapper`.
//!
//! All methods have default no-op implementations. Users implement only the
//! callbacks they care about.

use crate::api::types::*;
use crate::types::{InstrumentId, Quote, HistoricalTickData, RealTimeBar};

/// ibapi-compatible callback interface. Mirrors C++ `EWrapper`.
///
/// Implement the callbacks you care about; all default to no-ops.
#[allow(unused_variables)]
pub trait Wrapper {
    // ── Connection ──

    fn connect_ack(&mut self) {}
    fn connection_closed(&mut self) {}
    fn next_valid_id(&mut self, order_id: i64) {}
    fn managed_accounts(&mut self, accounts_list: &str) {}
    fn error(&mut self, req_id: i64, error_code: i64, error_string: &str, advanced_order_reject_json: &str) {}
    fn current_time(&mut self, time: i64) {}

    // ── Market Data ──

    fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, attrib: &TickAttrib) {}
    fn tick_size(&mut self, req_id: i64, tick_type: i32, size: f64) {}
    fn tick_string(&mut self, req_id: i64, tick_type: i32, value: &str) {}
    fn tick_generic(&mut self, req_id: i64, tick_type: i32, value: f64) {}
    fn tick_snapshot_end(&mut self, req_id: i64) {}
    fn market_data_type(&mut self, req_id: i64, market_data_type: i32) {}

    // ── Orders ──

    fn order_status(
        &mut self, order_id: i64, status: &str, filled: f64, remaining: f64,
        avg_fill_price: f64, perm_id: i64, parent_id: i64,
        last_fill_price: f64, client_id: i64, why_held: &str, mkt_cap_price: f64,
    ) {}

    fn open_order(&mut self, order_id: i64, contract: &Contract, order: &Order, order_state: &OrderState) {}
    fn open_order_end(&mut self) {}
    fn exec_details(&mut self, req_id: i64, contract: &Contract, execution: &Execution) {}
    fn exec_details_end(&mut self, req_id: i64) {}
    fn commission_report(&mut self, report: &CommissionReport) {}

    // ── Account ──

    fn update_account_value(&mut self, key: &str, value: &str, currency: &str, account_name: &str) {}
    fn update_portfolio(
        &mut self, contract: &Contract, position: f64, market_price: f64,
        market_value: f64, average_cost: f64, unrealized_pnl: f64,
        realized_pnl: f64, account_name: &str,
    ) {}
    fn update_account_time(&mut self, timestamp: &str) {}
    fn account_download_end(&mut self, account: &str) {}
    fn account_summary(&mut self, req_id: i64, account: &str, tag: &str, value: &str, currency: &str) {}
    fn account_summary_end(&mut self, req_id: i64) {}
    fn position(&mut self, account: &str, contract: &Contract, pos: f64, avg_cost: f64) {}
    fn position_end(&mut self) {}
    fn pnl(&mut self, req_id: i64, daily_pnl: f64, unrealized_pnl: f64, realized_pnl: f64) {}
    fn pnl_single(&mut self, req_id: i64, pos: f64, daily_pnl: f64, unrealized_pnl: f64, realized_pnl: f64, value: f64) {}

    // ── Historical Data ──

    fn historical_data(&mut self, req_id: i64, bar: &BarData) {}
    fn historical_data_end(&mut self, req_id: i64, start: &str, end: &str) {}
    fn historical_data_update(&mut self, req_id: i64, bar: &BarData) {}
    fn head_timestamp(&mut self, req_id: i64, head_timestamp: &str) {}

    // ── Contract Details ──

    fn contract_details(&mut self, req_id: i64, details: &ContractDetails) {}
    fn contract_details_end(&mut self, req_id: i64) {}
    fn symbol_samples(&mut self, req_id: i64, descriptions: &[ContractDescription]) {}

    // ── Tick-by-Tick ──

    fn tick_by_tick_all_last(
        &mut self, req_id: i64, tick_type: i32, time: i64, price: f64,
        size: f64, attrib: &TickAttribLast, exchange: &str, special_conditions: &str,
    ) {}
    fn tick_by_tick_bid_ask(
        &mut self, req_id: i64, time: i64, bid_price: f64, ask_price: f64,
        bid_size: f64, ask_size: f64, attrib: &TickAttribBidAsk,
    ) {}
    fn tick_by_tick_mid_point(&mut self, req_id: i64, time: i64, mid_point: f64) {}

    // ── Scanner ──

    fn scanner_data(
        &mut self, req_id: i64, rank: i32, details: &ContractDetails,
        distance: &str, benchmark: &str, projection: &str, legs_str: &str,
    ) {}
    fn scanner_data_end(&mut self, req_id: i64) {}
    fn scanner_parameters(&mut self, xml: &str) {}

    // ── News ──

    fn tick_news(
        &mut self, ticker_id: i64, timestamp: i64, provider_code: &str,
        article_id: &str, headline: &str, extra_data: &str,
    ) {}
    fn historical_news(
        &mut self, req_id: i64, time: &str, provider_code: &str,
        article_id: &str, headline: &str,
    ) {}
    fn historical_news_end(&mut self, req_id: i64, has_more: bool) {}
    fn news_article(&mut self, req_id: i64, article_type: i32, article_text: &str) {}

    // ── Real-Time Bars ──

    fn real_time_bar(
        &mut self, req_id: i64, date: i64, open: f64, high: f64,
        low: f64, close: f64, volume: f64, wap: f64, count: i32,
    ) {}

    // ── Historical Ticks ──

    fn historical_ticks(&mut self, req_id: i64, ticks: &HistoricalTickData, done: bool) {}

    // ── Histogram ──

    fn histogram_data(&mut self, req_id: i64, items: &[(f64, i64)]) {}

    // ── Market Rules ──

    fn market_rule(&mut self, market_rule_id: i64, price_increments: &[PriceIncrement]) {}

    // ── Completed Orders ──

    fn completed_order(&mut self, contract: &Contract, order: &Order, order_state: &OrderState) {}
    fn completed_orders_end(&mut self) {}

    // ── Historical Schedule ──

    fn historical_schedule(
        &mut self, req_id: i64, start_date_time: &str, end_date_time: &str,
        time_zone: &str, sessions: &[(String, String, String)],
    ) {}

    // ── Fundamental Data ──

    fn fundamental_data(&mut self, req_id: i64, data: &str) {}

    // ── Market Depth ──

    fn update_mkt_depth(
        &mut self, req_id: i64, position: i32, operation: i32,
        side: i32, price: f64, size: f64,
    ) {}
    fn update_mkt_depth_l2(
        &mut self, req_id: i64, position: i32, market_maker: &str,
        operation: i32, side: i32, price: f64, size: f64, is_smart_depth: bool,
    ) {}

    // ── Tick Req Params ──

    fn tick_req_params(&mut self, ticker_id: i64, min_tick: f64, bbo_exchange: &str, snapshot_permissions: i64) {}
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A Wrapper impl that records all callback invocations for testing.
    #[derive(Default)]
    pub struct RecordingWrapper {
        pub events: Vec<String>,
    }

    impl Wrapper for RecordingWrapper {
        fn connect_ack(&mut self) {
            self.events.push("connect_ack".into());
        }
        fn connection_closed(&mut self) {
            self.events.push("connection_closed".into());
        }
        fn next_valid_id(&mut self, order_id: i64) {
            self.events.push(format!("next_valid_id:{order_id}"));
        }
        fn error(&mut self, req_id: i64, error_code: i64, error_string: &str, _: &str) {
            self.events.push(format!("error:{req_id}:{error_code}:{error_string}"));
        }
        fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, _: &TickAttrib) {
            self.events.push(format!("tick_price:{req_id}:{tick_type}:{price}"));
        }
        fn tick_size(&mut self, req_id: i64, tick_type: i32, size: f64) {
            self.events.push(format!("tick_size:{req_id}:{tick_type}:{size}"));
        }
        fn order_status(
            &mut self, order_id: i64, status: &str, filled: f64, remaining: f64,
            avg_fill_price: f64, _: i64, _: i64, _: f64, _: i64, _: &str, _: f64,
        ) {
            self.events.push(format!("order_status:{order_id}:{status}:{filled}:{remaining}:{avg_fill_price}"));
        }
        fn exec_details(&mut self, req_id: i64, _contract: &Contract, execution: &Execution) {
            self.events.push(format!("exec_details:{req_id}:{}:{}", execution.side, execution.shares));
        }
        fn historical_data(&mut self, req_id: i64, bar: &BarData) {
            self.events.push(format!("historical_data:{req_id}:{}", bar.date));
        }
        fn historical_data_end(&mut self, req_id: i64, _: &str, _: &str) {
            self.events.push(format!("historical_data_end:{req_id}"));
        }
        fn contract_details(&mut self, req_id: i64, details: &ContractDetails) {
            self.events.push(format!("contract_details:{req_id}:{}", details.contract.symbol));
        }
        fn contract_details_end(&mut self, req_id: i64) {
            self.events.push(format!("contract_details_end:{req_id}"));
        }
        fn head_timestamp(&mut self, req_id: i64, ts: &str) {
            self.events.push(format!("head_timestamp:{req_id}:{ts}"));
        }
        fn tick_by_tick_all_last(
            &mut self, req_id: i64, tick_type: i32, time: i64, price: f64,
            size: f64, _: &TickAttribLast, exchange: &str, _: &str,
        ) {
            self.events.push(format!("tbt_last:{req_id}:{tick_type}:{time}:{price}:{size}:{exchange}"));
        }
        fn tick_by_tick_bid_ask(
            &mut self, req_id: i64, time: i64, bid_price: f64, ask_price: f64,
            bid_size: f64, ask_size: f64, _: &TickAttribBidAsk,
        ) {
            self.events.push(format!("tbt_bidask:{req_id}:{time}:{bid_price}:{ask_price}:{bid_size}:{ask_size}"));
        }
        fn position(&mut self, account: &str, contract: &Contract, pos: f64, avg_cost: f64) {
            self.events.push(format!("position:{account}:{}:{pos}:{avg_cost}", contract.con_id));
        }
        fn real_time_bar(
            &mut self, req_id: i64, date: i64, open: f64, high: f64,
            low: f64, close: f64, volume: f64, wap: f64, count: i32,
        ) {
            self.events.push(format!("real_time_bar:{req_id}:{date}:{open}:{high}:{low}:{close}"));
        }
        fn scanner_parameters(&mut self, _xml: &str) {
            self.events.push("scanner_parameters".into());
        }
        fn tick_news(
            &mut self, _: i64, _: i64, provider_code: &str, article_id: &str, headline: &str, _: &str,
        ) {
            self.events.push(format!("tick_news:{provider_code}:{article_id}:{headline}"));
        }
        fn histogram_data(&mut self, req_id: i64, items: &[(f64, i64)]) {
            self.events.push(format!("histogram_data:{req_id}:{}", items.len()));
        }
        fn market_rule(&mut self, id: i64, increments: &[PriceIncrement]) {
            self.events.push(format!("market_rule:{id}:{}", increments.len()));
        }
        fn fundamental_data(&mut self, req_id: i64, _data: &str) {
            self.events.push(format!("fundamental_data:{req_id}"));
        }
    }

    #[test]
    fn recording_wrapper_starts_empty() {
        let w = RecordingWrapper::default();
        assert!(w.events.is_empty());
    }

    #[test]
    fn recording_wrapper_records_connect_ack() {
        let mut w = RecordingWrapper::default();
        w.connect_ack();
        assert_eq!(w.events, vec!["connect_ack"]);
    }

    #[test]
    fn recording_wrapper_records_tick_price() {
        let mut w = RecordingWrapper::default();
        let attrib = TickAttrib::default();
        w.tick_price(1, 1, 150.25, &attrib);
        assert_eq!(w.events, vec!["tick_price:1:1:150.25"]);
    }

    #[test]
    fn recording_wrapper_records_order_status() {
        let mut w = RecordingWrapper::default();
        w.order_status(42, "Filled", 100.0, 0.0, 150.0, 0, 0, 150.0, 0, "", 0.0);
        assert_eq!(w.events, vec!["order_status:42:Filled:100:0:150"]);
    }

    #[test]
    fn recording_wrapper_records_exec_details() {
        let mut w = RecordingWrapper::default();
        let c = Contract::default();
        let e = Execution { side: "BOT".into(), shares: 100.0, ..Default::default() };
        w.exec_details(-1, &c, &e);
        assert_eq!(w.events, vec!["exec_details:-1:BOT:100"]);
    }

    #[test]
    fn recording_wrapper_records_historical_data() {
        let mut w = RecordingWrapper::default();
        let bar = BarData { date: "20260101".into(), ..Default::default() };
        w.historical_data(5, &bar);
        w.historical_data_end(5, "", "");
        assert_eq!(w.events, vec!["historical_data:5:20260101", "historical_data_end:5"]);
    }

    #[test]
    fn recording_wrapper_records_position() {
        let mut w = RecordingWrapper::default();
        let c = Contract { con_id: 265598, ..Default::default() };
        w.position("DU1234567", &c, 100.0, 150.25);
        assert_eq!(w.events, vec!["position:DU1234567:265598:100:150.25"]);
    }

    #[test]
    fn recording_wrapper_multiple_events() {
        let mut w = RecordingWrapper::default();
        w.connect_ack();
        w.next_valid_id(1);
        w.connection_closed();
        assert_eq!(w.events.len(), 3);
        assert_eq!(w.events[0], "connect_ack");
        assert_eq!(w.events[1], "next_valid_id:1");
        assert_eq!(w.events[2], "connection_closed");
    }

    /// Verify a bare no-op impl compiles — ensures all defaults work.
    struct NoOpWrapper;
    impl Wrapper for NoOpWrapper {}

    #[test]
    fn noop_wrapper_compiles() {
        let mut w = NoOpWrapper;
        w.connect_ack();
        w.tick_price(0, 0, 0.0, &TickAttrib::default());
        w.order_status(0, "", 0.0, 0.0, 0.0, 0, 0, 0.0, 0, "", 0.0);
        // If this compiles, all defaults are valid.
    }
}
