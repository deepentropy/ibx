//! Reference data: contract details, historical data, scanners, news, fundamentals.

use crate::types::*;

use super::{Contract, EClient, TagValue};
use crate::client_core::ClientCore;

impl EClient {
    // ── Historical Data ──

    /// Request historical data. Matches `reqHistoricalData` in C++.
    /// A request the reference refuses locally gets its error (321 or
    /// 10314) and no query; SCHEDULE asks for the trading schedule
    /// (ibx#430).
    pub fn req_historical_data(
        &self, req_id: i64, contract: &Contract,
        end_date_time: &str, duration: &str, bar_size: &str,
        what_to_show: &str, use_rth: bool, format_date: i32, keep_up_to_date: bool,
    ) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("req_historical_data", &[req_id, contract.con_id]) { return Ok(()); }
        if let Some((code, text)) = ClientCore::historical_refusal(end_date_time, duration, bar_size, what_to_show,
            format_date, keep_up_to_date, &contract.sec_type) {
            self.shared.reference.push_historical_error(req_id, code, text);
            return Ok(());
        }
        if what_to_show.eq_ignore_ascii_case("SCHEDULE") {
            return self.send(ClientCore::resolve_first(req_id, contract, ControlCommand::FetchHistoricalSchedule {
                req_id,
                con_id: contract.con_id,
                sec_type: contract.sec_type.clone(),
                exchange: contract.exchange.clone(),
                end_date_time: end_date_time.into(),
                duration: duration.into(),
                use_rth,
            }));
        }
        self.send(ClientCore::resolve_first(req_id, contract, ControlCommand::FetchHistorical {
            req_id,
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            end_date_time: end_date_time.into(),
            duration: duration.into(),
            bar_size: bar_size.into(),
            what_to_show: what_to_show.into(),
            use_rth,
            keep_up_to_date,
            include_expired: contract.include_expired,
            format_date,
        }))
    }

    /// Cancel historical data. Matches `cancelHistoricalData` in C++.
    pub fn cancel_historical_data(&self, req_id: i64) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("cancel_historical_data", &[req_id]) { return Ok(()); }
        self.send(ControlCommand::CancelHistorical { req_id })
    }

    /// Request head timestamp. Matches `reqHeadTimeStamp` in C++.
    pub fn req_head_time_stamp(
        &self, req_id: i64, contract: &Contract, what_to_show: &str, use_rth: bool, format_date: i32,
    ) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("req_head_time_stamp", &[req_id, contract.con_id]) { return Ok(()); }
        self.send(ClientCore::resolve_first(req_id, contract, ControlCommand::FetchHeadTimestamp {
            req_id,
            con_id: contract.con_id,
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            what_to_show: what_to_show.into(),
            use_rth,
            format_date,
        }))
    }

    // ── Contract Details ──

    /// Request contract details. Matches `reqContractDetails` in C++.
    pub fn req_contract_details(&self, req_id: i64, contract: &Contract) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("req_contract_details", &[req_id, contract.con_id]) { return Ok(()); }
        self.send(ControlCommand::FetchContractDetails {
            req_id,
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            currency: contract.currency.clone(),
            filters: crate::types::SecDefFilters {
                primary_exchange: contract.primary_exchange.clone(),
                local_symbol: contract.local_symbol.clone(),
                last_trade_date_or_contract_month: contract.last_trade_date_or_contract_month.clone(),
                strike: contract.strike,
                right: contract.right.clone(),
                multiplier: contract.multiplier.clone(),
                trading_class: contract.trading_class.clone(),
                sec_id: contract.sec_id.clone(),
                sec_id_type: contract.sec_id_type.clone(),
                include_expired: contract.include_expired,
            },
        })
    }

    /// Request available exchanges for market depth.
    pub fn req_mkt_depth_exchanges(&self) -> Result<(), String> {
        self.send(ControlCommand::FetchMktDepthExchanges)
    }

    /// Request matching symbols. Matches `reqMatchingSymbols` in C++.
    /// An empty or invalid pattern gives 321 and nothing is sent; the
    /// pattern is sent trimmed (ibx#439).
    pub fn req_matching_symbols(&self, req_id: i64, pattern: &str) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("req_matching_symbols", &[req_id]) { return Ok(()); }
        let pattern = match crate::client_core::matching_symbols_pattern(pattern) {
            Ok(pattern) => pattern,
            Err((code, message)) => {
                self.shared.orders.push_order_error(req_id, code, message);
                return Ok(());
            }
        };
        self.send(ControlCommand::FetchMatchingSymbols {
            req_id,
            pattern,
        })
    }

    /// Cancel head timestamp request. Matches `cancelHeadTimestamp` in C++.
    pub fn cancel_head_time_stamp(&self, req_id: i64) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("cancel_head_time_stamp", &[req_id]) { return Ok(()); }
        self.send(ControlCommand::CancelHeadTimestamp { req_id })
    }

    /// Request market rule by ID. Matches `reqMarketRule` in C++.
    /// Answered from the rules of the definition replies received; an id
    /// not received, or a rule with no price increments, gives 322
    /// (ibx#437).
    pub fn req_market_rule(&self, market_rule_id: i32, wrapper: &mut impl crate::api::wrapper::Wrapper) {
        match crate::client_core::market_rule_answer(self.shared.reference.market_rule(market_rule_id), market_rule_id) {
            Ok(increments) => wrapper.market_rule(market_rule_id as i64, &increments.iter()
                .map(|pi| crate::api::types::PriceIncrement { low_edge: pi.low_edge, increment: pi.increment })
                .collect::<Vec<_>>()),
            Err((code, message)) => wrapper.error(-1, code, &message, ""),
        }
    }

    // ── News Bulletins ──

    /// Subscribe to news bulletins. Matches `reqNewsBulletins` in C++:
    /// `all_msgs` replays the bulletins of the day first.
    pub fn req_news_bulletins(&self, all_msgs: bool) {
        self.core.subscribe_bulletins(all_msgs);
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

    /// Subscribe to a market scanner. Matches `reqScannerSubscription` in C++:
    /// the whole subscription, the subscription options and the filter
    /// options (ibx#456). A local refusal comes back through `error`.
    pub fn req_scanner_subscription(
        &self, req_id: i64, subscription: &crate::api::types::ScannerSubscription,
        scanner_subscription_options: &[TagValue],
        scanner_subscription_filter_options: &[TagValue],
    ) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("req_scanner_subscription", &[req_id]) { return Ok(()); }
        match ClientCore::scanner_request(subscription, scanner_subscription_options, scanner_subscription_filter_options) {
            Ok(subscription) => self.send(ControlCommand::SubscribeScanner {
                req_id,
                client_id: self.core.client_id.load(std::sync::atomic::Ordering::Relaxed),
                subscription,
            }),
            Err((code, text)) => {
                self.shared.orders.push_order_error(req_id, code, text);
                Ok(())
            }
        }
    }

    /// Cancel a scanner subscription. Matches `cancelScannerSubscription` in C++.
    pub fn cancel_scanner_subscription(&self, req_id: i64) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("cancel_scanner_subscription", &[req_id]) { return Ok(()); }
        self.send(ControlCommand::CancelScanner { req_id })
    }

    // ── News ──

    /// Request historical news headlines. Matches `reqHistoricalNews` in C++.
    /// The dates are sent as given; at most 300 headlines. A local refusal
    /// comes back through `error`.
    pub fn req_historical_news(
        &self, req_id: i64, con_id: i64, provider_codes: &str,
        start_time: &str, end_time: &str, max_results: u32,
    ) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("req_historical_news", &[req_id, con_id]) { return Ok(()); }
        let sources = self.shared.reference.news_sources();
        if let Some((code, text)) = ClientCore::historical_news_refusal(provider_codes, max_results as i64, &sources) {
            self.shared.orders.push_order_error(req_id, code, text);
            return Ok(());
        }
        self.send(ControlCommand::FetchHistoricalNews {
            req_id,
            con_id,
            provider_codes: provider_codes.into(),
            start_time: start_time.into(),
            end_time: end_time.into(),
            max_results: max_results.min(ClientCore::MAX_NEWS_RESULTS as u32),
        })
    }

    /// Request a news article by provider and article ID. Matches `reqNewsArticle` in C++.
    /// A local refusal comes back through `error`.
    pub fn req_news_article(&self, req_id: i64, provider_code: &str, article_id: &str) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("req_news_article", &[req_id]) { return Ok(()); }
        let sources = self.shared.reference.news_sources();
        if let Some((code, text)) = ClientCore::news_article_refusal(provider_code, article_id, &sources) {
            self.shared.orders.push_order_error(req_id, code, text);
            return Ok(());
        }
        self.send(ControlCommand::FetchNewsArticle {
            req_id,
            provider_code: provider_code.into(),
            article_id: article_id.into(),
        })
    }

    // ── Fundamental Data ──

    /// Request fundamental data (e.g. ReportSnapshot, ReportsFinSummary). Matches `reqFundamentalData` in C++.
    pub fn req_fundamental_data(&self, req_id: i64, contract: &Contract, report_type: &str) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("req_fundamental_data", &[req_id, contract.con_id]) { return Ok(()); }
        if let Some((code, text)) = ClientCore::fundamental_refusal(&contract.sec_type) {
            self.shared.reference.push_historical_error(req_id, code, text);
            return Ok(());
        }
        self.send(ClientCore::resolve_first(req_id, contract, ControlCommand::FetchFundamentalData {
            req_id,
            con_id: contract.con_id,
            report_type: report_type.into(),
        }))
    }

    /// Cancel fundamental data. Matches `cancelFundamentalData` in C++.
    pub fn cancel_fundamental_data(&self, req_id: i64) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("cancel_fundamental_data", &[req_id]) { return Ok(()); }
        self.send(ControlCommand::CancelFundamentalData { req_id })
    }

    // ── Histogram ──

    /// Request price histogram data. Matches `reqHistogramData` in C++.
    pub fn req_histogram_data(&self, req_id: i64, contract: &Contract, use_rth: bool, period: &str) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("req_histogram_data", &[req_id, contract.con_id]) { return Ok(()); }
        self.send(ClientCore::resolve_first(req_id, contract, ControlCommand::FetchHistogramData {
            req_id,
            con_id: contract.con_id,
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            use_rth,
            period: period.into(),
        }))
    }

    /// Cancel histogram data. Matches `cancelHistogramData` in C++.
    pub fn cancel_histogram_data(&self, req_id: i64) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("cancel_histogram_data", &[req_id]) { return Ok(()); }
        self.send(ControlCommand::CancelHistogramData { req_id })
    }

    // ── Historical Ticks ──

    /// Request historical tick data. Matches `reqHistoricalTicks` in C++.
    /// The reference's warnings (2174, 10299) and local refusals (10314,
    /// 321) come first (ibx#432).
    #[allow(clippy::too_many_arguments)]
    pub fn req_historical_ticks(
        &self, req_id: i64, contract: &Contract,
        start_date_time: &str, end_date_time: &str,
        number_of_ticks: i32, what_to_show: &str, use_rth: bool,
        ignore_size: bool, _misc_options: &[TagValue],
    ) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("req_historical_ticks", &[req_id, contract.con_id]) { return Ok(()); }
        let (answers, go_on) = ClientCore::historical_ticks_checks(start_date_time, end_date_time, number_of_ticks,
            what_to_show, ignore_size, &contract.sec_type, &contract.exchange);
        for (code, text) in answers {
            self.shared.reference.push_historical_error(req_id, code, text);
        }
        if !go_on {
            return Ok(());
        }
        let symbol = if contract.local_symbol.is_empty() { &contract.symbol } else { &contract.local_symbol };
        self.send(ClientCore::resolve_first(req_id, contract, ControlCommand::FetchHistoricalTicks {
            req_id,
            con_id: contract.con_id,
            symbol: symbol.clone(),
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            start_date_time: start_date_time.into(),
            end_date_time: end_date_time.into(),
            number_of_ticks,
            what_to_show: what_to_show.into(),
            use_rth,
            ignore_size,
        }))
    }

    // ── Historical Schedule ──

    /// Request historical trading schedule. Matches `reqHistoricalSchedule` in C++.
    pub fn req_historical_schedule(
        &self, req_id: i64, contract: &Contract,
        end_date_time: &str, duration: &str, use_rth: bool,
    ) -> Result<(), String> {
        if !crate::client_core::ClientCore::ids_fit("req_historical_schedule", &[req_id, contract.con_id]) { return Ok(()); }
        self.send(ClientCore::resolve_first(req_id, contract, ControlCommand::FetchHistoricalSchedule {
            req_id,
            con_id: contract.con_id,
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            end_date_time: end_date_time.into(),
            duration: duration.into(),
            use_rth,
        }))
    }
}
