//! Reference data: contract details, historical data, scanners, news, fundamentals.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::types::*;
use super::EClient;
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
        let tx = self.tx()?;
        let _ = (format_date, chart_options);
        if !what_to_show.eq_ignore_ascii_case("SCHEDULE") {
            ClientCore::validate_historical_args(bar_size_setting, what_to_show, keep_up_to_date)
                .map_err(|e| PyRuntimeError::new_err(e))?;
        }
        if what_to_show.eq_ignore_ascii_case("SCHEDULE") {
            Self::send_control(py, &tx, ControlCommand::FetchHistoricalSchedule {
                req_id: req_id as u32,
                con_id: contract.con_id,
                end_date_time: end_date_time.to_string(),
                duration: duration_str.to_string(),
                use_rth: use_rth != 0,
            })?;
        } else {
            Self::send_control(py, &tx, ControlCommand::FetchHistorical {
                req_id: req_id as u32,
                con_id: contract.con_id,
                symbol: contract.symbol.clone(),
                end_date_time: end_date_time.to_string(),
                duration: duration_str.to_string(),
                bar_size: bar_size_setting.to_string(),
                what_to_show: what_to_show.to_string(),
                use_rth: use_rth != 0,
                keep_up_to_date,
            })?;
        }
        Ok(())
    }

    /// Cancel historical data.
    fn cancel_historical_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::CancelHistorical { req_id: req_id as u32 })?;
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
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::FetchHeadTimestamp {
            req_id: req_id as u32,
            con_id: contract.con_id,
            what_to_show: what_to_show.to_string(),
            use_rth: use_rth != 0,
        })?;
        let _ = format_date;
        Ok(())
    }

    /// Cancel head timestamp request.
    fn cancel_head_time_stamp(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::CancelHeadTimestamp { req_id: req_id as u32 })?;
        Ok(())
    }

    /// Request contract details.
    fn req_contract_details(&self, py: Python<'_>, req_id: i64, contract: &Contract) -> PyResult<()> {
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::FetchContractDetails {
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
            },
        })?;
        Ok(())
    }

    /// Request available exchanges for market depth.
    fn req_mkt_depth_exchanges(&self, py: Python<'_>) -> PyResult<()> {
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::FetchMktDepthExchanges)?;
        Ok(())
    }

    /// Search for matching symbols.
    fn req_matching_symbols(&self, py: Python<'_>, req_id: i64, pattern: &str) -> PyResult<()> {
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::FetchMatchingSymbols {
            req_id: req_id as u32,
            pattern: pattern.to_string(),
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
            Self::send_control(py, &tx, ControlCommand::SubscribeScanner {
                req_id: req_id as u32, instrument, location_code, scan_code, max_items,
            })
        })
    }

    /// Cancel scanner subscription.
    fn cancel_scanner_subscription(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::CancelScanner { req_id: req_id as u32 })?;
        Ok(())
    }

    /// Request scanner parameters XML.
    fn req_scanner_parameters(&self, py: Python<'_>) -> PyResult<()> {
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::FetchScannerParams)?;
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
        let _ = news_article_options;
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::FetchNewsArticle {
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
        let _ = historical_news_options;
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::FetchHistoricalNews {
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
        let _ = fundamental_data_options;
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::FetchFundamentalData {
            req_id: req_id as u32,
            con_id: contract.con_id as u32,
            report_type: report_type.to_string(),
        })?;
        Ok(())
    }

    /// Cancel fundamental data.
    fn cancel_fundamental_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::CancelFundamentalData { req_id: req_id as u32 })?;
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
        let tx = self.tx()?;
        let _ = (ignore_size, misc_options);
        Self::send_control(py, &tx, ControlCommand::FetchHistoricalTicks {
            req_id: req_id as u32,
            con_id: contract.con_id,
            start_date_time: start_date_time.to_string(),
            end_date_time: end_date_time.to_string(),
            number_of_ticks: number_of_ticks as u32,
            what_to_show: what_to_show.to_string(),
            use_rth: use_rth != 0,
        })?;
        Ok(())
    }

    /// Request market rule details.
    fn req_market_rule(&self, py: Python<'_>, market_rule_id: i32) -> PyResult<()> {
        if let Some(shared) = self.shared.lock().unwrap().clone() {
            if let Some(rule) = shared.reference.market_rule(market_rule_id) {
                let increments: Vec<(f64, f64)> = rule.price_increments.iter()
                    .map(|pi| (pi.low_edge, pi.increment)).collect();
                let list = pyo3::types::PyList::new(py, increments.iter().map(|(low, inc)| {
                    pyo3::types::PyTuple::new(py, &[*low, *inc]).unwrap()
                }))?;
                self.wrapper.call_method1(py, "market_rule", (market_rule_id as i64, list.as_any()))?;
                return Ok(());
            }
        }
        log::warn!("req_market_rule: rule {} not in cache", market_rule_id);
        Ok(())
    }

    /// Request histogram data.
    #[pyo3(signature = (req_id, contract, use_rth, time_period))]
    fn req_histogram_data(&self, py: Python<'_>, req_id: i64, contract: &Contract, use_rth: bool, time_period: &str) -> PyResult<()> {
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::FetchHistogramData {
            req_id: req_id as u32,
            con_id: contract.con_id as u32,
            use_rth,
            period: time_period.to_string(),
        })?;
        Ok(())
    }

    /// Cancel histogram data.
    fn cancel_histogram_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::CancelHistogramData { req_id: req_id as u32 })?;
        Ok(())
    }

    /// Request historical trading schedule.
    #[pyo3(signature = (req_id, contract, end_date_time="", duration_str="1 M", use_rth=true))]
    fn req_historical_schedule(
        &self, py: Python<'_>, req_id: i64, contract: &Contract,
        end_date_time: &str, duration_str: &str, use_rth: bool,
    ) -> PyResult<()> {
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::FetchHistoricalSchedule {
            req_id: req_id as u32,
            con_id: contract.con_id,
            end_date_time: end_date_time.into(),
            duration: duration_str.into(),
            use_rth,
        })?;
        Ok(())
    }
}
