//! Reference data: contract details, historical data, scanners, news, fundamentals.

use pyo3::prelude::*;

use crate::types::*;
use super::{send_cmd, EClient};
use super::super::contract::Contract;
use crate::client_core::ClientCore;

#[pymethods]
impl EClient {
    /// Request historical bar data.
    #[pyo3(signature = (req_id, contract, end_date_time, duration_str, bar_size_setting, what_to_show, use_rth, format_date=1, keep_up_to_date=false, chart_options=Vec::new()))]
    fn req_historical_data(
        &self,
        py: Python<'_>,
        req_id: i64,
        contract: &Contract,
        end_date_time: &str,
        duration_str: &str,
        bar_size_setting: &str,
        what_to_show: &str,
        use_rth: i32,
        format_date: i32,
        keep_up_to_date: bool,
        chart_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_historical_data", &[req_id, contract.con_id]) { return Ok(()); }
        let tx = self.tx()?;
        let _ = chart_options;
        // A request the reference refuses locally gets its error (321 or
        // 10314) and no query (ibx#430).
        let shared = self.shared_state()?;
        if let Some((code, text)) = ClientCore::historical_refusal(end_date_time, duration_str, bar_size_setting,
            what_to_show, format_date, keep_up_to_date, &contract.sec_type, &contract.exchange, shared.reference.backfill_years_limit(),
            contract.include_expired) {
            shared.reference.push_historical_error(req_id, code, text);
            return Ok(());
        }
        if what_to_show.eq_ignore_ascii_case("SCHEDULE") {
            send_cmd(py, &tx, ClientCore::resolve_first(req_id, &contract.to_api(), ControlCommand::FetchHistoricalSchedule {
                req_id,
                con_id: contract.con_id,
                sec_type: contract.sec_type.clone(),
                exchange: contract.exchange.clone(),
                end_date_time: end_date_time.to_string(),
                duration: duration_str.to_string(),
                use_rth: use_rth != 0,
            }))?;
        } else {
            send_cmd(py, &tx, ClientCore::resolve_first(req_id, &contract.to_api(), ControlCommand::FetchHistorical {
                req_id,
                con_id: contract.con_id,
                symbol: contract.symbol.clone(),
                sec_type: contract.sec_type.clone(),
                exchange: contract.exchange.clone(),
                end_date_time: end_date_time.to_string(),
                duration: duration_str.to_string(),
                bar_size: bar_size_setting.to_string(),
                what_to_show: what_to_show.to_string(),
                use_rth: use_rth != 0,
                keep_up_to_date,
                include_expired: contract.include_expired,
                format_date,
            }))?;
        }
        Ok(())
    }

    /// Cancel historical data.
    fn cancel_historical_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("cancel_historical_data", &[req_id]) { return Ok(()); }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::CancelHistorical { req_id })?;
        Ok(())
    }

    /// Request head timestamp.
    #[pyo3(signature = (req_id, contract, what_to_show, use_rth, format_date=1))]
    fn req_head_time_stamp(
        &self,
        py: Python<'_>,
        req_id: i64,
        contract: &Contract,
        what_to_show: &str,
        use_rth: i32,
        format_date: i32,
    ) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_head_time_stamp", &[req_id, contract.con_id]) { return Ok(()); }
        let tx = self.tx()?;
        if let Some((code, text)) = ClientCore::head_timestamp_refusal(&contract.exchange) {
            self.shared_state()?.reference.push_historical_error(req_id, code, text);
            return Ok(());
        }
        send_cmd(py, &tx, ClientCore::resolve_first(req_id, &contract.to_api(), ControlCommand::FetchHeadTimestamp {
            req_id,
            con_id: contract.con_id,
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            what_to_show: what_to_show.to_string(),
            use_rth: use_rth != 0,
            format_date,
        }))?;
        Ok(())
    }

    /// Cancel head timestamp request.
    fn cancel_head_time_stamp(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("cancel_head_time_stamp", &[req_id]) { return Ok(()); }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::CancelHeadTimestamp { req_id })?;
        Ok(())
    }

    /// Request contract details.
    fn req_contract_details(&self, py: Python<'_>, req_id: i64, contract: &Contract) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_contract_details", &[req_id, contract.con_id]) { return Ok(()); }
        let tx = self.tx()?;
        if let Some((code, text)) = ClientCore::contract_details_refusal(&contract.to_api()) {
            self.shared_state()?.reference.push_historical_error(req_id, code as i32, text);
            return Ok(());
        }
        send_cmd(py, &tx, ControlCommand::FetchContractDetails {
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
                issuer_id: contract.issuer_id.clone(),
            },
        })?;
        Ok(())
    }

    /// Request available exchanges for market depth.
    fn req_mkt_depth_exchanges(&self, py: Python<'_>) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::FetchMktDepthExchanges)?;
        Ok(())
    }

    /// Search for matching symbols.
    fn req_matching_symbols(&self, py: Python<'_>, req_id: i64, pattern: &str) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_matching_symbols", &[req_id]) { return Ok(()); }
        // An empty or invalid pattern gives 321 and nothing is sent; the
        // pattern is sent trimmed (ibx#439).
        let allowed = self.shared_state()?.reference.matching_symbols_allowed();
        let pattern = match crate::client_core::matching_symbols_pattern(pattern, allowed) {
            Ok(pattern) => pattern,
            Err((code, message)) => {
                self.shared_state()?.orders.push_order_error(req_id, code, message);
                return Ok(());
            }
        };
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::FetchMatchingSymbols {
            req_id,
            pattern,
        })?;
        Ok(())
    }

    /// Request option chain parameters (ibx#440). A request the reference
    /// refuses locally gets 321; the rows come through
    /// `security_definition_option_parameter`, then
    /// `security_definition_option_parameter_end`.
    #[pyo3(signature = (req_id, underlying_symbol, fut_fop_exchange="", underlying_sec_type="STK", underlying_con_id=0))]
    fn req_sec_def_opt_params(
        &self,
        py: Python<'_>,
        req_id: i64,
        underlying_symbol: &str,
        fut_fop_exchange: &str,
        underlying_sec_type: &str,
        underlying_con_id: i64,
    ) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !ClientCore::ids_fit("req_sec_def_opt_params", &[req_id, underlying_con_id]) { return Ok(()); }
        if let Some((code, text)) = crate::control::optparams::refusal(underlying_sec_type, fut_fop_exchange, underlying_con_id) {
            self.shared_state()?.orders.push_order_error(req_id, code, text);
            return Ok(());
        }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::FetchSecDefOptParams {
            req_id,
            underlying_symbol: underlying_symbol.into(),
            fut_fop_exchange: fut_fop_exchange.into(),
            underlying_sec_type: crate::control::optparams::sec_type_name(underlying_sec_type).into(),
            underlying_con_id,
        })?;
        Ok(())
    }

    /// Request scanner subscription: the whole ibapi subscription, the
    /// subscription options and the filter options (ibx#456). A local
    /// refusal comes back through `error`.
    #[pyo3(signature = (req_id, subscription, scanner_subscription_options=Vec::new(), scanner_subscription_filter_options=Vec::new()))]
    fn req_scanner_subscription(
        &self,
        req_id: i64,
        subscription: Py<PyAny>,
        scanner_subscription_options: Vec<Py<PyAny>>,
        scanner_subscription_filter_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_scanner_subscription", &[req_id]) { return Ok(()); }
        let tx = self.tx()?;
        Python::attach(|py| {
            let text = |name: &str, default: &str| subscription.getattr(py, name)
                .and_then(|v| v.extract::<String>(py)).unwrap_or_else(|_| default.to_string());
            let double = |name: &str| subscription.getattr(py, name)
                .and_then(|v| v.extract::<f64>(py)).unwrap_or(f64::MAX);
            let int = |name: &str| subscription.getattr(py, name)
                .and_then(|v| v.extract::<i64>(py)).map(|v| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
                .unwrap_or(i32::MAX);
            // A text value follows the reference's reading: 1, true or yes.
            let exclude_convertible = subscription.getattr(py, "excludeConvertible").ok().is_some_and(|v| {
                v.extract::<bool>(py).unwrap_or_else(|_| {
                    v.extract::<String>(py).map(|t| matches!(t.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                        .unwrap_or(false)
                })
            });
            let sub = crate::api::types::ScannerSubscription {
                number_of_rows: subscription.getattr(py, "numberOfRows")
                    .and_then(|v| v.extract::<i32>(py)).unwrap_or(50),
                instrument: text("instrument", "STK"),
                location_code: text("locationCode", "STK.US.MAJOR"),
                scan_code: text("scanCode", "TOP_PERC_GAIN"),
                above_price: double("abovePrice"),
                below_price: double("belowPrice"),
                above_volume: int("aboveVolume"),
                market_cap_above: double("marketCapAbove"),
                market_cap_below: double("marketCapBelow"),
                moody_rating_above: text("moodyRatingAbove", ""),
                moody_rating_below: text("moodyRatingBelow", ""),
                sp_rating_above: text("spRatingAbove", ""),
                sp_rating_below: text("spRatingBelow", ""),
                maturity_date_above: text("maturityDateAbove", ""),
                maturity_date_below: text("maturityDateBelow", ""),
                coupon_rate_above: double("couponRateAbove"),
                coupon_rate_below: double("couponRateBelow"),
                exclude_convertible,
                average_option_volume_above: int("averageOptionVolumeAbove"),
                scanner_setting_pairs: text("scannerSettingPairs", ""),
                stock_type_filter: text("stockTypeFilter", ""),
            };
            let tag_values = |list: &[Py<PyAny>]| -> Vec<crate::api::types::TagValue> {
                list.iter().map(|tv| crate::api::types::TagValue {
                    tag: tv.getattr(py, "tag").and_then(|v| v.extract::<String>(py)).unwrap_or_default(),
                    value: tv.getattr(py, "value").and_then(|v| v.extract::<String>(py)).unwrap_or_default(),
                }).collect()
            };
            let (options, filter_options) = (tag_values(&scanner_subscription_options), tag_values(&scanner_subscription_filter_options));
            match ClientCore::scanner_request(&sub, &options, &filter_options) {
                Ok(subscription) => {
                    let client_id = self.core.client_id.load(std::sync::atomic::Ordering::Relaxed);
                    send_cmd(py, &tx, ControlCommand::SubscribeScanner { req_id, client_id, subscription })
                }
                Err((code, text)) => {
                    self.shared_state()?.orders.push_order_error(req_id, code, text);
                    Ok(())
                }
            }
        })
    }

    /// Cancel scanner subscription.
    fn cancel_scanner_subscription(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("cancel_scanner_subscription", &[req_id]) { return Ok(()); }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::CancelScanner { req_id })?;
        Ok(())
    }

    /// Request scanner parameters XML.
    fn req_scanner_parameters(&self, py: Python<'_>) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::FetchScannerParams)?;
        Ok(())
    }

    /// Request a news article.
    #[pyo3(signature = (req_id, provider_code, article_id, news_article_options=Vec::new()))]
    fn req_news_article(
        &self,
        py: Python<'_>,
        req_id: i64,
        provider_code: &str,
        article_id: &str,
        news_article_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_news_article", &[req_id]) { return Ok(()); }
        let _ = news_article_options;
        let tx = self.tx()?;
        let shared = self.shared_state()?;
        if let Some((code, text)) = ClientCore::news_article_refusal(provider_code, article_id, &shared.reference.news_sources()) {
            shared.orders.push_order_error(req_id, code, text);
            return Ok(());
        }
        send_cmd(py, &tx, ControlCommand::FetchNewsArticle {
            req_id,
            provider_code: provider_code.to_string(),
            article_id: article_id.to_string(),
        })?;
        Ok(())
    }

    /// Request historical news.
    #[pyo3(signature = (req_id, con_id, provider_codes, start_date_time, end_date_time, total_results, historical_news_options=Vec::new()))]
    fn req_historical_news(
        &self,
        py: Python<'_>,
        req_id: i64,
        con_id: i64,
        provider_codes: &str,
        start_date_time: &str,
        end_date_time: &str,
        total_results: i32,
        historical_news_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_historical_news", &[req_id, con_id]) { return Ok(()); }
        let _ = historical_news_options;
        let tx = self.tx()?;
        let shared = self.shared_state()?;
        if let Some((code, text)) = ClientCore::historical_news_refusal(provider_codes, total_results as i64, &shared.reference.news_sources()) {
            shared.orders.push_order_error(req_id, code, text);
            return Ok(());
        }
        send_cmd(py, &tx, ControlCommand::FetchHistoricalNews {
            req_id,
            con_id,
            provider_codes: provider_codes.to_string(),
            start_time: start_date_time.to_string(),
            end_time: end_date_time.to_string(),
            max_results: (total_results as i64).min(ClientCore::MAX_NEWS_RESULTS) as u32,
        })?;
        Ok(())
    }

    /// Request fundamental data.
    #[pyo3(signature = (req_id, contract, report_type, fundamental_data_options=Vec::new()))]
    fn req_fundamental_data(
        &self,
        py: Python<'_>,
        req_id: i64,
        contract: &Contract,
        report_type: &str,
        fundamental_data_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_fundamental_data", &[req_id, contract.con_id]) { return Ok(()); }
        let _ = fundamental_data_options;
        if let Some((code, text)) = ClientCore::fundamental_refusal(&contract.sec_type) {
            self.shared_state()?.reference.push_historical_error(req_id, code, text);
            return Ok(());
        }
        let tx = self.tx()?;
        send_cmd(py, &tx, ClientCore::resolve_first(req_id, &contract.to_api(), ControlCommand::FetchFundamentalData {
            req_id,
            con_id: contract.con_id,
            report_type: report_type.to_string(),
        }))?;
        Ok(())
    }

    /// Cancel fundamental data.
    fn cancel_fundamental_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("cancel_fundamental_data", &[req_id]) { return Ok(()); }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::CancelFundamentalData { req_id })?;
        Ok(())
    }

    /// Request historical tick data.
    #[pyo3(signature = (req_id, contract, start_date_time="", end_date_time="", number_of_ticks=1000, what_to_show="TRADES", use_rth=1, ignore_size=false, misc_options=Vec::new()))]
    fn req_historical_ticks(
        &self,
        py: Python<'_>,
        req_id: i64,
        contract: &Contract,
        start_date_time: &str,
        end_date_time: &str,
        number_of_ticks: i32,
        what_to_show: &str,
        use_rth: i32,
        ignore_size: bool,
        misc_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_historical_ticks", &[req_id, contract.con_id]) { return Ok(()); }
        let tx = self.tx()?;
        let _ = misc_options;
        // The reference's warnings and local refusals first (ibx#432).
        let (answers, go_on) = ClientCore::historical_ticks_checks(start_date_time, end_date_time, number_of_ticks,
            what_to_show, ignore_size, &contract.sec_type, &contract.exchange);
        let shared = self.shared_state()?;
        for (code, text) in answers {
            shared.reference.push_historical_error(req_id, code, text);
        }
        if !go_on {
            return Ok(());
        }
        let symbol = if contract.local_symbol.is_empty() { &contract.symbol } else { &contract.local_symbol };
        send_cmd(py, &tx, ClientCore::resolve_first(req_id, &contract.to_api(), ControlCommand::FetchHistoricalTicks {
            req_id,
            con_id: contract.con_id,
            symbol: symbol.clone(),
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            start_date_time: start_date_time.to_string(),
            end_date_time: end_date_time.to_string(),
            number_of_ticks,
            what_to_show: what_to_show.to_string(),
            use_rth: use_rth != 0,
            ignore_size,
        }))?;
        Ok(())
    }

    /// Request market rule details.
    fn req_market_rule(&self, py: Python<'_>, market_rule_id: i32) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if self.later(move |this, py| this.req_market_rule(py, market_rule_id)) { return Ok(()); }
        let rule = self.shared.lock().unwrap().clone()
            .and_then(|shared| shared.reference.market_rule(market_rule_id));
        // An id not received, or a rule with no price increments: 322
        // (ibx#437).
        match crate::client_core::market_rule_answer(rule, market_rule_id) {
            Ok(increments) => {
                let list = pyo3::types::PyList::new(py, increments.iter().map(|pi| {
                    pyo3::types::PyTuple::new(py, &[pi.low_edge, pi.increment]).unwrap()
                }))?;
                self.wrapper.call_method1(py, "market_rule", (market_rule_id as i64, list.as_any()))?;
            }
            Err((code, message)) => {
                self.wrapper.call_method1(py, "error", (-1i64, code, message.as_str(), ""))?;
            }
        }
        Ok(())
    }

    /// Request histogram data.
    #[pyo3(signature = (req_id, contract, use_rth, time_period))]
    fn req_histogram_data(&self, py: Python<'_>, req_id: i64, contract: &Contract, use_rth: bool, time_period: &str) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_histogram_data", &[req_id, contract.con_id]) { return Ok(()); }
        let tx = self.tx()?;
        send_cmd(py, &tx, ClientCore::resolve_first(req_id, &contract.to_api(), ControlCommand::FetchHistogramData {
            req_id,
            con_id: contract.con_id,
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            use_rth,
            period: time_period.to_string(),
        }))?;
        Ok(())
    }

    /// Cancel histogram data.
    fn cancel_histogram_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("cancel_histogram_data", &[req_id]) { return Ok(()); }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::CancelHistogramData { req_id })?;
        Ok(())
    }

    /// Request historical trading schedule.
    #[pyo3(signature = (req_id, contract, end_date_time="", duration_str="1 M", use_rth=true))]
    fn req_historical_schedule(
        &self, py: Python<'_>, req_id: i64, contract: &Contract,
        end_date_time: &str, duration_str: &str, use_rth: bool,
    ) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_historical_schedule", &[req_id, contract.con_id]) { return Ok(()); }
        let tx = self.tx()?;
        send_cmd(py, &tx, ClientCore::resolve_first(req_id, &contract.to_api(), ControlCommand::FetchHistoricalSchedule {
            req_id,
            con_id: contract.con_id,
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            end_date_time: end_date_time.into(),
            duration: duration_str.into(),
            use_rth,
        }))?;
        Ok(())
    }
}
