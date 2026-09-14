//! Reference data: contract details, historical data, scanners, news, fundamentals.

use pyo3::prelude::*;
use crate::error_codes::Refusal;

use crate::types::*;
use super::{wire_req_id, EClient};
use super::super::contract::Contract;
use crate::client_core::ClientCore;

#[pymethods]
impl EClient {
    /// Request historical bar data.
    ///
    /// `chart_options` is taken and not applied. This protocol's request
    /// carries no free-form option list, so what a caller puts in one cannot be
    /// sent. The reference client's own list is empty on every ordinary call.
    #[pyo3(signature = (req_id, contract, end_date_time, duration_str, bar_size_setting, what_to_show, use_rth, format_date=1, keep_up_to_date=false, chart_options=Vec::new()))]
    pub(crate) fn req_historical_data(
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
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        // How this request wants its bar times written, as on the other
        // surface: the venue states one form and the caller may want the other.
        self.core.note_date_format(req_id, format_date);
        // And what its range is counted from, which the reply does not state.
        self.core.note_historical_span(req_id, end_date_time, duration_str);
        let _ = chart_options;
        // Whatever finished under this id before, this is a new request.
        if let Ok(wire) = wire_req_id(req_id) {
            self.core.historical_request_is_new(wire);
        }
        if !what_to_show.eq_ignore_ascii_case("SCHEDULE")
            && let Err(why) = ClientCore::validate_historical_args(
                bar_size_setting, what_to_show, keep_up_to_date,
            )
        {
            return self.report_refusal(py, req_id, why.into());
        }
        // Named by the venue where the caller named it by id alone: a
        // request states the contract's type and its exchange, and both
        // are the venue's to say.
        let Some(by_venue) = self.named_or_report(py, req_id, contract)? else { return Ok(()) };
        let contract = &*by_venue;
        if what_to_show.eq_ignore_ascii_case("SCHEDULE") {
            if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchHistoricalSchedule {
                    contract: contract.into(),
                    req_id: wire_req_id(req_id)?,
                    end_date_time: end_date_time.to_string(),
                    duration: duration_str.to_string(),
                    use_rth: use_rth != 0,
                    filters: contract.lookup_filters(),
                }) {
                return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
            }
        } else {
            if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchHistorical {
                    contract: contract.into(),
                    req_id: wire_req_id(req_id)?,
                    end_date_time: end_date_time.to_string(),
                    duration: duration_str.to_string(),
                    bar_size: bar_size_setting.to_string(),
                    what_to_show: what_to_show.to_string(),
                    use_rth: use_rth != 0,
                    keep_up_to_date,
                    include_expired: contract.include_expired,
                    filters: contract.lookup_filters(),
                }) {
                return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
            }
        }
        Ok(())
    }

    /// Cancel historical data.
    fn cancel_historical_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        let wire = wire_req_id(req_id)?;
        // A withdrawn stream leaves nothing running under this id.
        self.core.historical_request_is_new(wire);
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::CancelHistorical { req_id: wire }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request head timestamp.
    #[pyo3(signature = (req_id, contract, what_to_show, use_rth, format_date=1))]
    pub(crate) fn req_head_time_stamp(
        &self,
        py: Python<'_>,
        req_id: i64,
        contract: &Contract,
        what_to_show: &str,
        use_rth: i32,
        format_date: i32,
    ) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        // Named by the venue where the caller named it by id alone: a
        // request states the contract's type and its exchange, and both
        // are the venue's to say.
        let Some(by_venue) = self.named_or_report(py, req_id, contract)? else { return Ok(()) };
        let contract = &*by_venue;
        // Noted before the request goes out, as on the other surface: an
        // answer dispatched between the send and the note was written in the
        // wrong form.
        self.core.note_date_format(req_id, format_date);
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchHeadTimestamp {
                contract: contract.into(),
                req_id: wire_req_id(req_id)?,
                what_to_show: what_to_show.to_string(),
                use_rth: use_rth != 0,
                include_expired: contract.include_expired,
                filters: contract.lookup_filters(),
            }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Cancel head timestamp request.
    fn cancel_head_time_stamp(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::CancelHeadTimestamp { req_id: wire_req_id(req_id)? }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request contract details.
    pub(crate) fn req_contract_details(&self, py: Python<'_>, req_id: i64, contract: &Contract) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchContractDetails {
                contract: contract.into(),
                req_id: wire_req_id(req_id)?,
                filters: contract.lookup_filters(),
            }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request available exchanges for market depth.
    fn req_mkt_depth_exchanges(&self, py: Python<'_>) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(-1)? else { return Ok(()) };
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchMktDepthExchanges) {
            return self.report_refusal(py, -1, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Search for matching symbols.
    pub(crate) fn req_matching_symbols(&self, py: Python<'_>, req_id: i64, pattern: &str) -> PyResult<()> {
        super::wire_text("a matching-symbols pattern", pattern)?;
        // Normalised and checked the way the request surface does it: the
        // same pattern reaches the same search service over the same wire,
        // and agreeing on one surface and not the other answers the same
        // call two ways.
        let pattern = crate::api::client::reference::matching_symbols_pattern(pattern)
            .map_err(|refusal| pyo3::exceptions::PyRuntimeError::new_err(refusal.message))?;
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchMatchingSymbols {
                req_id: wire_req_id(req_id)?,
                pattern,
            }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request option chain parameters.
    ///
    /// Every argument is stated by the caller, as the reference client requires
    /// them to be. With `underlying_sec_type` defaulted to stocks, a caller who
    /// left it off asked about the chains of a stock by that name rather than
    /// being told they had left it off.
    #[pyo3(signature = (req_id, underlying_symbol, fut_fop_exchange, underlying_sec_type, underlying_con_id))]
    pub(crate) fn req_sec_def_opt_params(
        &self,
        py: Python<'_>,
        req_id: i64,
        underlying_symbol: &str,
        fut_fop_exchange: &str,
        underlying_sec_type: &str,
        underlying_con_id: i64,
    ) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchOptionParams {
                req_id: wire_req_id(req_id)?,
                symbol: underlying_symbol.to_string(),
                fut_fop_exchange: fut_fop_exchange.to_string(),
                underlying_sec_type: underlying_sec_type.to_string(),
                underlying_con_id,
            }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request scanner subscription.
    ///
    /// `scanner_subscription_options` is taken and not applied. This protocol's
    /// request carries no free-form option list, so what a caller puts in one
    /// cannot be sent. The reference client's own list is empty on every
    /// ordinary call.
    #[pyo3(signature = (req_id, subscription, scanner_subscription_options=Vec::new(), scanner_subscription_filter_options=Vec::new()))]
    fn req_scanner_subscription(
        &self,
        req_id: i64,
        subscription: Py<PyAny>,
        scanner_subscription_options: Vec<Py<PyAny>>,
        scanner_subscription_filter_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        let _ = scanner_subscription_options;
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        Python::attach(|py| {
            // An absent attribute takes the default. One that is present and
            // cannot be read is a value the caller stated, and is refused
            // rather than run as a different scan under their request id.
            macro_rules! stated {
                ($attr:literal, $kind:ty, $default:expr) => {
                    match subscription.getattr(py, $attr) {
                        Err(_) => $default,
                        Ok(held) if held.is_none(py) => $default,
                        Ok(held) => match held.extract::<$kind>(py) {
                            Ok(value) => value,
                            Err(why) => return self.report_refusal(py, req_id,
                                crate::error_codes::Refusal::validation(format!(
                                    "the scan's {} cannot be read: {why}. Left off it would \
                                     be the default, but stated it names the scan, and one \
                                     run under a different one is not the scan asked for",
                                    $attr,
                                ))),
                        },
                    }
                };
            }
            // Empty, which is what the reference client's own subscription
            // holds for a field nobody set. Named as stocks and top gainers
            // here, a scan nobody described ran as a different scan.
            let instrument = stated!("instrument", String, String::new());
            let location_code = stated!("locationCode", String, String::new());
            let scan_code = stated!("scanCode", String, String::new());
            // Their own "no number stated" is a negative one, which is not a
            // count and is not an unreadable value either: it means the venue
            // picks, and so does its absence here.
            let rows = stated!("numberOfRows", i64, -1);
            let max_items = if rows < 0 { 50 } else { rows.min(u32::MAX as i64) as u32 };
            let filters = match scanner_filters(py, &subscription, &scanner_subscription_filter_options) {
                Ok(filters) => filters,
                Err(why) => return self.report_refusal(py, req_id, why),
            };
            Self::send_control(py, &tx, ControlCommand::SubscribeScanner {
                req_id: wire_req_id(req_id)?, instrument, location_code, scan_code, max_items, filters,
            })
        })
    }

    /// Cancel scanner subscription.
    fn cancel_scanner_subscription(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::CancelScanner { req_id: wire_req_id(req_id)? }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request scanner parameters XML.
    fn req_scanner_parameters(&self, py: Python<'_>) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(-1)? else { return Ok(()) };
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchScannerParams) {
            return self.report_refusal(py, -1, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request a news article.
    ///
    /// `news_article_options` is taken and not applied. This protocol's request
    /// carries no free-form option list, so what a caller puts in one cannot be
    /// sent. The reference client's own list is empty on every ordinary call.
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
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchNewsArticle {
                req_id: wire_req_id(req_id)?,
                provider_code: provider_code.to_string(),
                article_id: article_id.to_string(),
            }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request historical news.
    ///
    /// Bounds are UTC timestamps, `YYYYMMDD-HH:MM:SS` or `YYYYMMDD HH:MM:SS`,
    /// optionally with fractional seconds. Empty bounds are omitted; unreadable
    /// ones are refused so the window is not lost.
    ///
    /// `historical_news_options` is taken and not applied. This protocol's
    /// request carries no free-form option list, so what a caller puts in one
    /// cannot be sent. The reference client's own list is empty on every
    /// ordinary call.
    #[pyo3(signature = (req_id, con_id, provider_codes, start_date_time, end_date_time, total_results, historical_news_options=Vec::new()))]
    pub(crate) fn req_historical_news(
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
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        if let Err(why) = crate::control::news::validate_news_window(
            start_date_time, end_date_time,
        ) {
            return self.report_refusal(py, req_id, why.into());
        }
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchHistoricalNews {
                req_id: wire_req_id(req_id)?,
                con_id: super::wire_con_id("a request for headlines", con_id)?,
                provider_codes: provider_codes.to_string(),
                start_time: start_date_time.to_string(),
                end_time: end_date_time.to_string(),
                // No more than the reference client asks for, whatever was wanted.
                max_results: super::wire_u32("total_results", total_results as i64)?
                    .min(crate::control::news::MOST_HEADLINES_ASKED_FOR),
            }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }


    /// Ask for a contract's corporate actions over a range of days.
    ///
    /// The venue answers per contract, not per request, so the answer is filed
    /// against the contract it names. `corporate_actions` asks and waits in one
    /// call; this is the request on its own.
    #[pyo3(signature = (req_id, con_id, sec_type, exchange, start_date, end_date))]
    pub(crate) fn req_adjustments(
        &self,
        py: Python<'_>,
        req_id: i64,
        con_id: i64,
        sec_type: &str,
        exchange: &str,
        start_date: &str,
        end_date: &str,
    ) -> PyResult<()> {
        // Refused for every caller. The answering calls number themselves in a
        // band of their own and hold a number while they wait, so a request
        // numbered inside it has its answer taken by one of those calls, about
        // a contract and a range it did not ask for. That call sends its own
        // command directly and does not come through here.
        if crate::bridge::ReferenceState::is_ask_id(super::wire_u32("req_id", req_id)?) {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "req_id {req_id} is inside the range this client numbers its own \
                 answering calls in, and an answer under it would be taken for one of \
                 theirs: number the request below {}",
                crate::bridge::ReferenceState::ASK_ID_BASE,
            )));
        }
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        // Narrowed the way the request surface narrows it: a contract id of
        // zero, or a negative one, names nothing and the venue answers it with
        // silence — which reads as a contract with no actions rather than a
        // question that was never askable.
        let con_id = u32::try_from(con_id).ok().filter(|id| *id > 0).ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err(format!(
                "corporate actions are asked for by the venue's id for the contract, \
                 and {con_id} is not one: qualify the contract first and pass what \
                 comes back",
            ))
        })?;
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchAdjustments {
                req_id: wire_req_id(req_id)?,
                con_id,
                sec_type: sec_type.to_string(),
                exchange: exchange.to_string(),
                start_date: start_date.to_string(),
                end_date: end_date.to_string(),
            }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request fundamental data.
    ///
    /// `fundamental_data_options` is taken and not applied. This protocol's
    /// request carries no free-form option list, so what a caller puts in one
    /// cannot be sent. The reference client's own list is empty on every
    /// ordinary call.
    #[pyo3(signature = (req_id, contract, report_type, fundamental_data_options=Vec::new()))]
    pub(crate) fn req_fundamental_data(
        &self,
        py: Python<'_>,
        req_id: i64,
        contract: &Contract,
        report_type: &str,
        fundamental_data_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        let _ = fundamental_data_options;
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchFundamentalData {
                req_id: wire_req_id(req_id)?,
                con_id: super::wire_con_id("a request for a fundamental report", contract.con_id)?,
                report_type: report_type.to_string(),
            }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Cancel fundamental data.
    fn cancel_fundamental_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::CancelFundamentalData { req_id: wire_req_id(req_id)? }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request historical tick data.
    ///
    /// `ignore_size` and `misc_options` are taken and not applied. The request
    /// has no field for suppressing size-only changes, and none for a free-form
    /// option list.
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
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        let _ = (ignore_size, misc_options);
        if let Err(why) = crate::control::historical::tick_data_type(what_to_show)
            .map(|_| ())
            .and_then(|()| crate::control::historical::validate_tick_window(
                start_date_time, end_date_time,
            ))
        {
            return self.report_refusal(py, req_id, why.into());
        }
        // Named by the venue where the caller named it by id alone: a
        // request states the contract's type and its exchange, and both
        // are the venue's to say.
        let Some(by_venue) = self.named_or_report(py, req_id, contract)? else { return Ok(()) };
        let contract = &*by_venue;
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchHistoricalTicks {
                contract: contract.into(),
                req_id: wire_req_id(req_id)?,
                start_date_time: start_date_time.to_string(),
                end_date_time: end_date_time.to_string(),
                number_of_ticks: super::wire_u32("number_of_ticks", number_of_ticks as i64)?,
                what_to_show: what_to_show.to_string(),
                use_rth: use_rth != 0,
                include_expired: contract.include_expired,
                filters: contract.lookup_filters(),
            }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request market rule details.
    fn req_market_rule(&self, py: Python<'_>, market_rule_id: i32) -> PyResult<()> {
        let Some(_connected) = self.tx_or_report(market_rule_id as i64)? else { return Ok(()) };
        // Released before the callback below — see the note in
        // req_completed_orders.
        let shared = self.shared.lock().unwrap().clone();
        if let Some(shared) = shared
            && let Some(rule) = shared.reference.market_rule(market_rule_id) {
                // Objects with names on them, as the reference client hands
                // them over and as the Rust surface here already did. A pair of
                // numbers left a program reading `lowEdge` holding a tuple.
                let steps: Vec<Py<pyo3::PyAny>> = rule.price_increments.iter()
                    .map(|pi| Py::new(py, super::super::contract::PriceIncrementPy {
                        low_edge: pi.low_edge,
                        increment: pi.increment,
                    }).map(|o| o.into_any()))
                    .collect::<PyResult<_>>()?;
                let list = pyo3::types::PyList::new(py, steps)?;
                self.deliver(py, "market_rule", (market_rule_id as i64, list.as_any()))?;
                return Ok(());
            }
        // Answered, not logged. A caller waiting on a callback that will never
        // come cannot tell that apart from a slow venue, and the other client
        // here has answered this all along.
        //
        // Under the number and against the id the reference client reports a
        // miss under: 322, against -1. The rule's number is not a request
        // number — nothing was ever sent under it — and this is not a
        // malformed request, so neither the id nor the validation code the
        // refusals above carry is the one a caller branching on the pair
        // reads there.
        self.report_refusal(
            py,
            -1,
            crate::error_codes::Refusal::stated(322, format!(
                "market rule {market_rule_id} has not been seen on this session. Rules \
                 arrive with the details of a contract that uses them, so ask for such a \
                 contract first"
            )),
        )
    }

    /// Request histogram data.
    #[pyo3(signature = (req_id, contract, use_rth, time_period))]
    pub(crate) fn req_histogram_data(&self, py: Python<'_>, req_id: i64, contract: &Contract, use_rth: bool, time_period: &str) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        // Named by the venue where the caller named it by id alone: a
        // request states the contract's type and its exchange, and both
        // are the venue's to say.
        let Some(by_venue) = self.named_or_report(py, req_id, contract)? else { return Ok(()) };
        let contract = &*by_venue;
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchHistogramData {
                req_id: wire_req_id(req_id)?,
                con_id: super::wire_con_id("a request for a histogram", contract.con_id)?,
                sec_type: contract.sec_type.clone(),
                exchange: contract.exchange.clone(),
                use_rth,
                period: time_period.to_string(),
            }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Cancel histogram data.
    fn cancel_histogram_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::CancelHistogramData { req_id: wire_req_id(req_id)? }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request historical trading schedule.
    #[pyo3(signature = (req_id, contract, end_date_time="", duration_str="1 M", use_rth=true))]
    pub(crate) fn req_historical_schedule(
        &self, py: Python<'_>, req_id: i64, contract: &Contract,
        end_date_time: &str, duration_str: &str, use_rth: bool,
    ) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        // Named by the venue where the caller named it by id alone: a
        // request states the contract's type and its exchange, and both
        // are the venue's to say.
        let Some(by_venue) = self.named_or_report(py, req_id, contract)? else { return Ok(()) };
        let contract = &*by_venue;
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::FetchHistoricalSchedule {
                contract: contract.into(),
                req_id: wire_req_id(req_id)?,
                end_date_time: end_date_time.into(),
                duration: duration_str.into(),
                use_rth,
                filters: contract.lookup_filters(),
            }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }
}

/// `ScannerSubscription` attribute -> the scanner filter code it selects. Everything a
/// caller sets beyond instrument / location / scan code is a filter, and a filter the
/// subscription drops is a different result set.
const SCANNER_FILTERS: &[(&str, &str)] = &[
    ("abovePrice", "priceAbove"),
    ("belowPrice", "priceBelow"),
    ("aboveVolume", "volumeAbove"),
    ("marketCapAbove", "marketCapAbove1e6"),
    ("marketCapBelow", "marketCapBelow1e6"),
    ("moodyRatingAbove", "moodyRatingAbove"),
    ("moodyRatingBelow", "moodyRatingBelow"),
    ("spRatingAbove", "spRatingAbove"),
    ("spRatingBelow", "spRatingBelow"),
    ("maturityDateAbove", "maturityDateAbove"),
    ("maturityDateBelow", "maturityDateBelow"),
    ("couponRateAbove", "couponRateAbove"),
    ("couponRateBelow", "couponRateBelow"),
    ("averageOptionVolumeAbove", "avgOptVolumeAbove"),
];

/// `stockTypeFilter` name -> its filter value. Anything else, `ALL` included, is no
/// filter.
fn stk_types_code(name: &str) -> &'static str {
    match name.to_ascii_uppercase().as_str() {
        "STOCK" => "exc:ETF",
        "ETF" => "inc:ETF",
        "CORP" => "inc:CORP",
        "ADR" => "inc:ADR",
        "REIT" => "inc:REIT",
        "CEF" => "inc:CEF",
        _ => "",
    }
}

/// One filter value, or `None` when the attribute is missing or left at its unset
/// default. One that is present and cannot be read is a refusal, not an unset
/// filter: sent without it, the scan would run narrower or wider than the one
/// described.
fn scanner_filter_value(py: Python<'_>, sub: &Py<PyAny>, attr: &str) -> Result<Option<String>, crate::error_codes::Refusal> {
    let Ok(value) = sub.getattr(py, attr) else { return Ok(None) };
    if value.is_none(py) {
        return Ok(None);
    }
    if let Ok(n) = value.extract::<f64>(py) {
        // An unset numeric filter arrives as `sys.float_info.max` or `2**31 - 1`, and
        // sending either as a bound would empty the scan.
        if n == f64::MAX || n == f64::from(i32::MAX) {
            return Ok(None);
        }
        return Ok(Some(n.to_string()));
    }
    if let Ok(text) = value.extract::<String>(py) {
        return Ok((!text.is_empty()).then_some(text));
    }
    Err(unreadable_filter(attr))
}

/// What a filter that is stated and cannot be read is reported as. Left off,
/// it would be no filter; stated it narrows the scan, and one run without it
/// is not the scan asked for.
fn unreadable_filter(what: &str) -> crate::error_codes::Refusal {
    crate::error_codes::Refusal::validation(format!(
        "the scan's {what} is stated but cannot be read, and a scan run \
         without it is not the scan asked for",
    ))
}

/// Collect the subscription's filters, then the caller's explicit filter tags, which
/// win
/// over the named attribute selecting the same code.
fn scanner_filters(py: Python<'_>, sub: &Py<PyAny>, filter_options: &[Py<PyAny>]) -> Result<Vec<(String, String)>, crate::error_codes::Refusal> {
    let mut filters: Vec<(String, String)> = Vec::new();
    for (attr, code) in SCANNER_FILTERS {
        if let Some(value) = scanner_filter_value(py, sub, attr)? {
            filters.push(((*code).to_string(), value));
        }
    }

    match sub.getattr(py, "excludeConvertible") {
        Err(_) => {}
        Ok(v) if v.is_none(py) => {}
        Ok(v) => match v.extract::<bool>(py) {
            Ok(true) => filters.push(("excludeConvertible".to_string(), "true".to_string())),
            Ok(false) => {}
            Err(_) => return Err(unreadable_filter("excludeConvertible")),
        },
    }
    match sub.getattr(py, "stockTypeFilter") {
        Err(_) => {}
        Ok(v) if v.is_none(py) => {}
        Ok(v) => {
            let Ok(name) = v.extract::<String>(py) else {
                return Err(unreadable_filter("stockTypeFilter"));
            };
            let stk_types = stk_types_code(&name);
            if !stk_types.is_empty() {
                filters.push(("stkTypes".to_string(), stk_types.to_string()));
            }
        }
    }

    // The pairs a caller sets the scan's own controls with — the sort column
    // and its direction among them. This protocol's scan carries a filter list
    // and no free-form settings field, so a pair stated here cannot be sent.
    // Dropped in silence, a scan asked for sorted one way came back sorted
    // another and the caller had nothing to tell them apart by.
    match sub.getattr(py, "scannerSettingPairs") {
        Err(_) => {}
        Ok(v) if v.is_none(py) => {}
        Ok(v) => match v.extract::<String>(py) {
            Ok(pairs) if pairs.is_empty() => {}
            Ok(pairs) => return Err(crate::error_codes::Refusal::validation(format!(
                "the scan states settings {pairs:?}, which this request cannot carry: a scan \
                 run without them is not the scan that was asked for",
            ))),
            Err(_) => return Err(unreadable_filter("scannerSettingPairs")),
        },
    }

    for (at, option) in filter_options.iter().enumerate() {
        let what = format!("filter tag at position {at}");
        let Ok(tag) = option.getattr(py, "tag") else {
            return Err(unreadable_filter(&what));
        };
        let Ok(tag) = tag.extract::<String>(py) else {
            return Err(unreadable_filter(&what));
        };
        if tag.is_empty() {
            continue;
        }
        let value = match option.getattr(py, "value") {
            Err(_) => return Err(unreadable_filter(&format!("value of {tag}"))),
            Ok(v) => v,
        };
        let Ok(value) = value.extract::<String>(py) else {
            return Err(unreadable_filter(&format!("value of {tag}")));
        };
        filters.retain(|(code, _)| *code != tag);
        filters.push((tag, value));
    }
    Ok(filters)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn namespace(py: Python<'_>, fields: &str) -> Py<PyAny> {
        py.eval(&std::ffi::CString::new(format!("__import__('types').SimpleNamespace({fields})")).unwrap(), None, None)
            .unwrap().unbind()
    }

    #[test]
    fn a_described_stock_can_request_adjusted_history() {
        Python::initialize();
        Python::attach(|py| {
            let client = EClient::__new__(&pyo3::types::PyTuple::empty(py), None);
            let wrapper = py.eval(
                c"__import__('builtins').type('W', (), {'__init__': lambda s: setattr(s, 'calls', []), '__getattr__': lambda s, n: (lambda *a: s.calls.append((n, a)))})()",
                None, None,
            ).unwrap().unbind();
            client.__init__(wrapper.clone_ref(py)).unwrap();
            let (tx, rx) = std::sync::mpsc::sync_channel(16);
            *client.control_tx.lock().unwrap() = Some(tx);
            *client.shared.lock().unwrap() = Some(std::sync::Arc::new(crate::bridge::SharedState::new()));
            client.connected.store(true, std::sync::atomic::Ordering::Release);
            let contract = Contract {
                symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(),
                currency: "USD".into(), primary_exchange: "NASDAQ".into(), ..Default::default()
            };

            client.req_historical_data(
                py, 7, &contract, "", "1 M", "1 day", "ADJUSTED_LAST", 1, 1, false, Vec::new(),
            ).unwrap();

            let ControlCommand::FetchHistorical {
                req_id, contract: sent, what_to_show, filters, ..
            } = rx.try_recv().expect("the engine resolves the description before asking for bars") else {
                panic!("the request asks for historical bars");
            };
            assert_eq!(req_id, 7);
            assert_eq!(sent, ContractRef::from(&contract));
            assert_eq!(sent.con_id, 0);
            assert_eq!(filters.primary_exchange, "NASDAQ");
            assert_eq!(what_to_show, "ADJUSTED_LAST");
            assert!(wrapper.getattr(py, "calls").unwrap().cast_bound::<pyo3::types::PyList>(py).unwrap().is_empty());
        });
    }

    #[test]
    fn scanner_subscription_attributes_become_filters() {
        Python::initialize();
        Python::attach(|py| {
            let sub = namespace(py, "abovePrice=10.0, belowPrice=1.7976931348623157e+308, \
                aboveVolume=2147483647, marketCapAbove=1.7976931348623157e+308, \
                moodyRatingAbove='', spRatingAbove='A', averageOptionVolumeAbove=500, \
                excludeConvertible=True, stockTypeFilter='etf'");
            assert_eq!(scanner_filters(py, &sub, &[]).unwrap(), vec![
                ("priceAbove".to_string(), "10".to_string()),
                ("spRatingAbove".to_string(), "A".to_string()),
                ("avgOptVolumeAbove".to_string(), "500".to_string()),
                ("excludeConvertible".to_string(), "true".to_string()),
                ("stkTypes".to_string(), "inc:ETF".to_string()),
            ]);
        });
    }

    #[test]
    fn explicit_filter_tags_replace_the_attribute_for_the_same_code() {
        Python::initialize();
        Python::attach(|py| {
            let sub = namespace(py, "abovePrice=10.0, excludeConvertible=False, stockTypeFilter='ALL'");
            let options = [
                namespace(py, "tag='priceAbove', value='20'"),
                namespace(py, "tag='usdMarketCapAbove', value='10000'"),
            ];
            assert_eq!(scanner_filters(py, &sub, &options).unwrap(), vec![
                ("priceAbove".to_string(), "20".to_string()),
                ("usdMarketCapAbove".to_string(), "10000".to_string()),
            ]);
        });
    }

    /// A setting this request cannot carry is refused, not dropped.
    ///
    /// The pairs set the scan's own controls — the sort column and its
    /// direction among them. Dropped in silence, a scan asked for sorted one
    /// way came back sorted another, with nothing to tell the two apart by.
    #[test]
    fn a_scanner_setting_that_cannot_be_sent_refuses_the_scan() {
        Python::initialize();
        Python::attach(|py| {
            let sub = namespace(py, "scannerSettingPairs='Annual,true'");
            let why = scanner_filters(py, &sub, &[])
                .expect_err("a stated setting is not dropped");
            assert!(why.message.contains("Annual,true"), "{}", why.message);

            // What the reference client's own subscription holds when nobody
            // set it, which states nothing and is no refusal.
            let sub = namespace(py, "scannerSettingPairs=''");
            assert!(scanner_filters(py, &sub, &[]).is_ok(), "an unset setting is not a refusal");
        });
    }

    /// A filter that is stated and cannot be read narrows the scan it is
    /// dropped from, and a scan run narrower is not the one asked for.
    /// Refused rather than dropped.
    #[test]
    fn a_filter_that_cannot_be_read_refuses_the_scan() {
        Python::initialize();
        Python::attach(|py| {
            let sub = namespace(py, "abovePrice=[1, 2]");
            let why = scanner_filters(py, &sub, &[])
                .expect_err("a stated filter that cannot be read is not dropped");
            assert!(why.message.contains("abovePrice"), "{}", why.message);

            let sub = namespace(py, "");
            let options = [namespace(py, "tag='priceAbove', value=10")];
            assert!(scanner_filters(py, &sub, &options).is_err(),
                "and neither is an explicit filter whose value cannot be read");
        });
    }

    /// A contract id of zero names no contract, and this surface narrows it
    /// the way the request surface does: the venue answers a question about
    /// contract zero with silence, which reads as a contract with no news.
    #[test]
    fn a_contract_id_of_zero_is_refused_where_the_request_surface_refuses_it() {
        Python::initialize();
        Python::attach(|py| {
            let client = EClient::__new__(&pyo3::types::PyTuple::empty(py), None);
            let ns = pyo3::types::PyDict::new(py);
            py.run(
                c"class W:\n    def __getattr__(self, name):\n        return lambda *args: None\nw = W()",
                None,
                Some(&ns),
            ).unwrap();
            let wrapper = ns.get_item("w").unwrap().unwrap().unbind();
            client.__init__(wrapper).unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(16);
            *client.control_tx.lock().unwrap() = Some(tx);

            let err = client
                .req_historical_news(py, 1, 0, "BRFG", "", "", 10, Vec::new())
                .expect_err("a contract id of zero names no contract");
            assert!(err.to_string().contains("is not one"), "{err}");
        });
    }

    /// The pattern rides the wire as one field's value, so this surface
    /// refuses one carrying the byte that separates fields the way the
    /// request surface does: sent anyway, what follows the byte would go out
    /// as fields the request never stated.
    #[test]
    fn a_pattern_carrying_the_field_separator_is_refused() {
        Python::initialize();
        Python::attach(|py| {
            let client = EClient::__new__(&pyo3::types::PyTuple::empty(py), None);
            let ns = pyo3::types::PyDict::new(py);
            py.run(
                c"class W:\n    def __getattr__(self, name):\n        return lambda *args: None\nw = W()",
                None,
                Some(&ns),
            ).unwrap();
            let wrapper = ns.get_item("w").unwrap().unwrap().unbind();
            client.__init__(wrapper).unwrap();

            let err = client
                .req_matching_symbols(py, 1, "AAPL\x011=999")
                .expect_err("a pattern cannot carry the byte that separates fields");
            assert!(err.to_string().contains("separates fields"), "{err}");
        });
    }

    /// The pattern goes out as the venue would have sent it, on this surface
    /// as on the other, and one it would not have sent at all is refused
    /// here rather than asked.
    #[test]
    fn a_matching_symbols_pattern_is_sent_as_the_venue_sends_it() {
        Python::initialize();
        Python::attach(|py| {
            let client = EClient::__new__(&pyo3::types::PyTuple::empty(py), None);
            let wrapper = py.eval(
                c"__import__('builtins').type('W', (), {'__init__': lambda s: setattr(s, 'calls', []), '__getattr__': lambda s, n: (lambda *a: s.calls.append((n, a)))})()",
                None, None,
            ).unwrap().unbind();
            client.__init__(wrapper).unwrap();
            let (tx, rx) = std::sync::mpsc::sync_channel(16);
            *client.control_tx.lock().unwrap() = Some(tx);
            *client.shared.lock().unwrap() = Some(std::sync::Arc::new(crate::bridge::SharedState::new()));
            client.connected.store(true, std::sync::atomic::Ordering::Release);

            client.req_matching_symbols(py, 8, "  APPLE   INC ").unwrap();
            let ControlCommand::FetchMatchingSymbols { pattern, .. } =
                rx.try_recv().expect("the search is asked for") else {
                panic!("the request asks the search service");
            };
            assert_eq!(pattern, "APPLE INC", "trimmed, and its runs of spaces collapsed");

            let err = client
                .req_matching_symbols(py, 8, "   ")
                .expect_err("the venue refuses this rather than answering it");
            assert!(err.to_string().contains("visible characters"), "{err}");
            assert!(rx.try_recv().is_err(), "and nothing was asked");
        });
    }

    /// A rule this session has not been told about is reported against -1
    /// under 322, the way the reference client reports one. The rule's number is
    /// not a request number, and a caller keying off the pair branches on
    /// both halves.
    #[test]
    fn an_unseen_market_rule_is_reported_against_minus_one_under_322() {
        Python::initialize();
        Python::attach(|py| {
            let client = EClient::__new__(&pyo3::types::PyTuple::empty(py), None);
            let wrapper = py.eval(
                c"__import__('builtins').type('W', (), {'__init__': lambda s: setattr(s, 'calls', []), '__getattr__': lambda s, n: (lambda *a: s.calls.append((n, a)))})()",
                None, None,
            ).unwrap().unbind();
            client.__init__(wrapper.clone_ref(py)).unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(16);
            *client.control_tx.lock().unwrap() = Some(tx);

            client.req_market_rule(py, 26).unwrap();

            let calls = wrapper.getattr(py, "calls").unwrap();
            let calls = calls.bind(py).cast::<pyo3::types::PyList>().unwrap();
            assert_eq!(calls.len(), 1, "the miss is answered, once");
            let (name, args): (String, Vec<Py<PyAny>>) = calls.get_item(0).unwrap().extract().unwrap();
            assert_eq!(name, "error");
            assert_eq!(args[0].extract::<i64>(py).unwrap(), -1,
                "the rule's number is not the number this is reported against");
            assert_eq!(args[2].extract::<i32>(py).unwrap(), 322);
            assert!(args[3].extract::<String>(py).unwrap().contains("market rule 26"));
        });
    }
}
