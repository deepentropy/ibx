//! Reference data: contract details, historical data, scanners, news, fundamentals.

use pyo3::exceptions::PyRuntimeError;
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
        if let Some(r) = self.not_connected(req_id as i64) { return r; }
        let tx = self.tx()?;
        let _ = chart_options;
        // A request the reference refuses locally gets its error (321 or
        // 10314) and no query (ibx#430).
        if let Some((code, text)) = ClientCore::historical_refusal(end_date_time, duration_str, bar_size_setting, what_to_show, format_date) {
            self.shared_state()?.reference.push_historical_error(req_id as u32, code, text);
            return Ok(());
        }
        ClientCore::validate_historical_args(bar_size_setting, what_to_show, keep_up_to_date)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        if what_to_show.eq_ignore_ascii_case("SCHEDULE") {
            send_cmd(py, &tx, ClientCore::resolve_first(req_id as u32, &contract.to_api(), ControlCommand::FetchHistoricalSchedule {
                req_id: req_id as u32,
                con_id: contract.con_id,
                sec_type: contract.sec_type.clone(),
                exchange: contract.exchange.clone(),
                end_date_time: end_date_time.to_string(),
                duration: duration_str.to_string(),
                use_rth: use_rth != 0,
            }))?;
        } else {
            send_cmd(py, &tx, ClientCore::resolve_first(req_id as u32, &contract.to_api(), ControlCommand::FetchHistorical {
                req_id: req_id as u32,
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
            }))?;
        }
        Ok(())
    }

    /// Cancel historical data.
    fn cancel_historical_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::CancelHistorical { req_id: req_id as u32 })?;
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
        if let Some(r) = self.not_connected(req_id as i64) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ClientCore::resolve_first(req_id as u32, &contract.to_api(), ControlCommand::FetchHeadTimestamp {
            req_id: req_id as u32,
            con_id: contract.con_id,
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            what_to_show: what_to_show.to_string(),
            use_rth: use_rth != 0,
        }))?;
        let _ = format_date;
        Ok(())
    }

    /// Cancel head timestamp request.
    fn cancel_head_time_stamp(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::CancelHeadTimestamp { req_id: req_id as u32 })?;
        Ok(())
    }

    /// Request contract details.
    fn req_contract_details(&self, py: Python<'_>, req_id: i64, contract: &Contract) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::FetchContractDetails {
            req_id: req_id as u32,
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
        })?;
        Ok(())
    }

    /// Request available exchanges for market depth.
    fn req_mkt_depth_exchanges(&self, py: Python<'_>) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::FetchMktDepthExchanges)?;
        Ok(())
    }

    /// Search for matching symbols.
    fn req_matching_symbols(&self, py: Python<'_>, req_id: i64, pattern: &str) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        // An empty or invalid pattern gives 321 and nothing is sent; the
        // pattern is sent trimmed (ibx#439).
        let pattern = match crate::client_core::matching_symbols_pattern(pattern) {
            Ok(pattern) => pattern,
            Err((code, message)) => {
                self.shared_state()?.orders.push_order_error(req_id as u64, code, message);
                return Ok(());
            }
        };
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::FetchMatchingSymbols {
            req_id: req_id as u32,
            pattern,
        })?;
        Ok(())
    }

    /// Request scanner subscription.
    #[pyo3(signature = (req_id, subscription, scanner_subscription_options=Vec::new()))]
    fn req_scanner_subscription(
        &self,
        req_id: i64,
        subscription: Py<PyAny>,
        scanner_subscription_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        if let Some(r) = self.not_connected(req_id as i64) { return r; }
        let _ = scanner_subscription_options;
        let tx = self.tx()?;
        Python::attach(|py| {
            let instrument = subscription.getattr(py, "instrument")
                .and_then(|v| v.extract::<String>(py)).unwrap_or_else(|_| "STK".to_string());
            let location_code = subscription.getattr(py, "locationCode")
                .and_then(|v| v.extract::<String>(py)).unwrap_or_else(|_| "STK.US.MAJOR".to_string());
            let scan_code = subscription.getattr(py, "scanCode")
                .and_then(|v| v.extract::<String>(py)).unwrap_or_else(|_| "TOP_PERC_GAIN".to_string());
            let max_items = subscription.getattr(py, "numberOfRows")
                .and_then(|v| v.extract::<u32>(py)).unwrap_or(50);
            let client_id = self.core.client_id.load(std::sync::atomic::Ordering::Relaxed);
            send_cmd(py, &tx, ControlCommand::SubscribeScanner {
                req_id: req_id as u32, client_id, instrument, location_code, scan_code, max_items,
            })
        })
    }

    /// Cancel scanner subscription.
    fn cancel_scanner_subscription(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::CancelScanner { req_id: req_id as u32 })?;
        Ok(())
    }

    /// Request scanner parameters XML.
    fn req_scanner_parameters(&self, py: Python<'_>) -> PyResult<()> {
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
        if let Some(r) = self.not_connected(req_id as i64) { return r; }
        let _ = news_article_options;
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::FetchNewsArticle {
            req_id: req_id as u32,
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
        if let Some(r) = self.not_connected(req_id as i64) { return r; }
        let _ = historical_news_options;
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::FetchHistoricalNews {
            req_id: req_id as u32,
            con_id: con_id as u32,
            provider_codes: provider_codes.to_string(),
            start_time: start_date_time.to_string(),
            end_time: end_date_time.to_string(),
            max_results: total_results as u32,
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
        if let Some(r) = self.not_connected(req_id as i64) { return r; }
        let _ = fundamental_data_options;
        let tx = self.tx()?;
        send_cmd(py, &tx, ClientCore::resolve_first(req_id as u32, &contract.to_api(), ControlCommand::FetchFundamentalData {
            req_id: req_id as u32,
            con_id: contract.con_id as u32,
            report_type: report_type.to_string(),
        }))?;
        Ok(())
    }

    /// Cancel fundamental data.
    fn cancel_fundamental_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(req_id as i64) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::CancelFundamentalData { req_id: req_id as u32 })?;
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
        if let Some(r) = self.not_connected(req_id as i64) { return r; }
        let tx = self.tx()?;
        let _ = (ignore_size, misc_options);
        send_cmd(py, &tx, ClientCore::resolve_first(req_id as u32, &contract.to_api(), ControlCommand::FetchHistoricalTicks {
            req_id: req_id as u32,
            con_id: contract.con_id,
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            start_date_time: start_date_time.to_string(),
            end_date_time: end_date_time.to_string(),
            number_of_ticks: number_of_ticks as u32,
            what_to_show: what_to_show.to_string(),
            use_rth: use_rth != 0,
        }))?;
        Ok(())
    }

    /// Request market rule details.
    fn req_market_rule(&self, py: Python<'_>, market_rule_id: i32) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
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
        if let Some(r) = self.not_connected(req_id as i64) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ClientCore::resolve_first(req_id as u32, &contract.to_api(), ControlCommand::FetchHistogramData {
            req_id: req_id as u32,
            con_id: contract.con_id as u32,
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            use_rth,
            period: time_period.to_string(),
        }))?;
        Ok(())
    }

    /// Cancel histogram data.
    fn cancel_histogram_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::CancelHistogramData { req_id: req_id as u32 })?;
        Ok(())
    }

    /// Request historical trading schedule.
    #[pyo3(signature = (req_id, contract, end_date_time="", duration_str="1 M", use_rth=true))]
    fn req_historical_schedule(
        &self, py: Python<'_>, req_id: i64, contract: &Contract,
        end_date_time: &str, duration_str: &str, use_rth: bool,
    ) -> PyResult<()> {
        if let Some(r) = self.not_connected(req_id as i64) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ClientCore::resolve_first(req_id as u32, &contract.to_api(), ControlCommand::FetchHistoricalSchedule {
            req_id: req_id as u32,
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
