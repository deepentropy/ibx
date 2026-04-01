//! Reference data: contract details, historical data, scanners, news, fundamentals.

use crate::types::*;

use super::{Contract, EClient};

impl EClient {
    // ── Historical Data ──

    /// Request historical data. Matches `reqHistoricalData` in C++.
    pub fn req_historical_data(
        &self, req_id: i64, contract: &Contract,
        end_date_time: &str, duration: &str, bar_size: &str,
        what_to_show: &str, use_rth: bool, _format_date: i32, keep_up_to_date: bool,
    ) -> Result<(), String> {
        self.send(ControlCommand::FetchHistorical {
            req_id: req_id as u32,
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            end_date_time: end_date_time.into(),
            duration: duration.into(),
            bar_size: bar_size.into(),
            what_to_show: what_to_show.into(),
            use_rth,
            keep_up_to_date,
        })
    }

    /// Cancel historical data. Matches `cancelHistoricalData` in C++.
    pub fn cancel_historical_data(&self, req_id: i64) -> Result<(), String> {
        self.send(ControlCommand::CancelHistorical { req_id: req_id as u32 })
    }

    /// Request head timestamp. Matches `reqHeadTimeStamp` in C++.
    pub fn req_head_time_stamp(
        &self, req_id: i64, contract: &Contract, what_to_show: &str, use_rth: bool, _format_date: i32,
    ) -> Result<(), String> {
        self.send(ControlCommand::FetchHeadTimestamp {
            req_id: req_id as u32,
            con_id: contract.con_id,
            what_to_show: what_to_show.into(),
            use_rth,
        })
    }

    // ── Contract Details ──

    /// Request contract details. Matches `reqContractDetails` in C++.
    pub fn req_contract_details(&self, req_id: i64, contract: &Contract) -> Result<(), String> {
        self.send(ControlCommand::FetchContractDetails {
            req_id: req_id as u32,
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            currency: contract.currency.clone(),
        })
    }

    /// Request available exchanges for market depth.
    pub fn req_mkt_depth_exchanges(&self) -> Result<(), String> {
        self.send(ControlCommand::FetchMktDepthExchanges)
    }

    /// Request matching symbols. Matches `reqMatchingSymbols` in C++.
    pub fn req_matching_symbols(&self, req_id: i64, pattern: &str) -> Result<(), String> {
        self.send(ControlCommand::FetchMatchingSymbols {
            req_id: req_id as u32,
            pattern: pattern.into(),
        })
    }

    /// Cancel head timestamp request. Matches `cancelHeadTimestamp` in C++.
    pub fn cancel_head_time_stamp(&self, req_id: i64) -> Result<(), String> {
        self.send(ControlCommand::CancelHeadTimestamp { req_id: req_id as u32 })
    }

    /// Request market rule by ID. Matches `reqMarketRule` in C++.
    /// Looks up cached market rules delivered during connection init.
    pub fn req_market_rule(&self, market_rule_id: i32, wrapper: &mut impl crate::api::wrapper::Wrapper) {
        if let Some(rule) = self.shared.reference.market_rule(market_rule_id) {
            wrapper.market_rule(market_rule_id as i64, &rule.price_increments.iter()
                .map(|pi| crate::api::types::PriceIncrement { low_edge: pi.low_edge, increment: pi.increment })
                .collect::<Vec<_>>());
        }
    }

    // ── News Bulletins ──

    /// Subscribe to news bulletins. Matches `reqNewsBulletins` in C++.
    pub fn req_news_bulletins(&self, _all_msgs: bool) {
        self.core.subscribe_bulletins();
    }

    /// Cancel news bulletin subscription. Matches `cancelNewsBulletins` in C++.
    pub fn cancel_news_bulletins(&self) {
        self.core.unsubscribe_bulletins();
    }

    // ── Scanner ──

    /// Request scanner parameters XML. Matches `reqScannerParameters` in C++.
    pub fn req_scanner_parameters(&self) -> Result<(), String> {
        self.send(ControlCommand::FetchScannerParams)
    }

    /// Subscribe to a market scanner. Matches `reqScannerSubscription` in C++.
    pub fn req_scanner_subscription(
        &self, req_id: i64, instrument: &str, location_code: &str,
        scan_code: &str, max_items: u32,
    ) -> Result<(), String> {
        self.send(ControlCommand::SubscribeScanner {
            req_id: req_id as u32,
            instrument: instrument.into(),
            location_code: location_code.into(),
            scan_code: scan_code.into(),
            max_items,
        })
    }

    /// Cancel a scanner subscription. Matches `cancelScannerSubscription` in C++.
    pub fn cancel_scanner_subscription(&self, req_id: i64) -> Result<(), String> {
        self.send(ControlCommand::CancelScanner { req_id: req_id as u32 })
    }

    // ── News ──

    /// Request historical news headlines. Matches `reqHistoricalNews` in C++.
    pub fn req_historical_news(
        &self, req_id: i64, con_id: i64, provider_codes: &str,
        start_time: &str, end_time: &str, max_results: u32,
    ) -> Result<(), String> {
        self.send(ControlCommand::FetchHistoricalNews {
            req_id: req_id as u32,
            con_id: con_id as u32,
            provider_codes: provider_codes.into(),
            start_time: start_time.into(),
            end_time: end_time.into(),
            max_results,
        })
    }

    /// Request a news article by provider and article ID. Matches `reqNewsArticle` in C++.
    pub fn req_news_article(&self, req_id: i64, provider_code: &str, article_id: &str) -> Result<(), String> {
        self.send(ControlCommand::FetchNewsArticle {
            req_id: req_id as u32,
            provider_code: provider_code.into(),
            article_id: article_id.into(),
        })
    }

    // ── Fundamental Data ──

    /// Request fundamental data (e.g. ReportSnapshot, ReportsFinSummary). Matches `reqFundamentalData` in C++.
    pub fn req_fundamental_data(&self, req_id: i64, contract: &Contract, report_type: &str) -> Result<(), String> {
        self.send(ControlCommand::FetchFundamentalData {
            req_id: req_id as u32,
            con_id: contract.con_id as u32,
            report_type: report_type.into(),
        })
    }

    /// Cancel fundamental data. Matches `cancelFundamentalData` in C++.
    pub fn cancel_fundamental_data(&self, req_id: i64) -> Result<(), String> {
        self.send(ControlCommand::CancelFundamentalData { req_id: req_id as u32 })
    }

    // ── Histogram ──

    /// Request price histogram data. Matches `reqHistogramData` in C++.
    pub fn req_histogram_data(&self, req_id: i64, contract: &Contract, use_rth: bool, period: &str) -> Result<(), String> {
        self.send(ControlCommand::FetchHistogramData {
            req_id: req_id as u32,
            con_id: contract.con_id as u32,
            use_rth,
            period: period.into(),
        })
    }

    /// Cancel histogram data. Matches `cancelHistogramData` in C++.
    pub fn cancel_histogram_data(&self, req_id: i64) -> Result<(), String> {
        self.send(ControlCommand::CancelHistogramData { req_id: req_id as u32 })
    }

    // ── Historical Ticks ──

    /// Request historical tick data. Matches `reqHistoricalTicks` in C++.
    pub fn req_historical_ticks(
        &self, req_id: i64, contract: &Contract,
        start_date_time: &str, end_date_time: &str,
        number_of_ticks: i32, what_to_show: &str, use_rth: bool,
    ) -> Result<(), String> {
        self.send(ControlCommand::FetchHistoricalTicks {
            req_id: req_id as u32,
            con_id: contract.con_id,
            start_date_time: start_date_time.into(),
            end_date_time: end_date_time.into(),
            number_of_ticks: number_of_ticks as u32,
            what_to_show: what_to_show.into(),
            use_rth,
        })
    }

    // ── Historical Schedule ──

    /// Request historical trading schedule. Matches `reqHistoricalSchedule` in C++.
    pub fn req_historical_schedule(
        &self, req_id: i64, contract: &Contract,
        end_date_time: &str, duration: &str, use_rth: bool,
    ) -> Result<(), String> {
        self.send(ControlCommand::FetchHistoricalSchedule {
            req_id: req_id as u32,
            con_id: contract.con_id,
            end_date_time: end_date_time.into(),
            duration: duration.into(),
            use_rth,
        })
    }
}
