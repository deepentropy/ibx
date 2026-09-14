//! Gateway-local fakes and pure no-op stubs.

use crate::error_codes::Refusal;
use crate::python::compat::client::wire_req_id;
use crate::types::ControlCommand;
use pyo3::prelude::*;

use super::EClient;
use super::super::contract::{Contract, NewsProviderPy, SmartComponentPy, SoftDollarTierPy};

#[pymethods]
impl EClient {
    // ── What the venue permits ──

    /// Security type → the order types the venue permits for it, as stated at
    /// logon. Empty until the session is up.
    fn order_permissions(&self) -> PyResult<std::collections::HashMap<String, Vec<String>>> {
        Ok(self.shared_state().map(|s| s.reference.order_permissions()).unwrap_or_default())
    }

    /// The order types permitted for one security type, or `None` when the
    /// type is not permitted at all. A combination is named `COMB`.
    fn permitted_order_types(&self, sec_type: &str) -> PyResult<Option<Vec<String>>> {
        Ok(self.shared_state()
            .ok()
            .and_then(|s| s.reference.permitted_order_types(&sec_type.to_ascii_uppercase())))
    }

    /// Feature tokens the venue enables for this account.
    fn enabled_features(&self) -> PyResult<Vec<String>> {
        Ok(self.shared_state().map(|s| s.reference.enabled_features()).unwrap_or_default())
    }

    /// Which algorithms the venue offers, keyed `PROVIDER/SECTYPE`.
    fn algorithms(&self) -> PyResult<std::collections::HashMap<String, Vec<String>>> {
        Ok(self.shared_state().map(|s| s.reference.algorithms()).unwrap_or_default())
    }

    /// The algorithms offered for one security type, across every provider.
    fn algorithms_for(&self, sec_type: &str) -> PyResult<Vec<String>> {
        Ok(self.shared_state().map(|s| s.reference.algorithms_for(sec_type)).unwrap_or_default())
    }

    /// The sets of order defaults this account holds, as `(key, version)`.
    ///
    /// The venue keeps one per security type and fills parts of an order the
    /// caller left unstated from them, so the same call on two accounts is not
    /// the same order. The key is the venue's own and the version is what that
    /// set is on; the values in a set are asked for separately.
    fn order_presets(&self) -> PyResult<Vec<(String, String)>> {
        Ok(self.shared_state().map(|s| s.reference.order_presets()).unwrap_or_default())
    }

    /// What the venue states about a contract's issuer on one series, as the
    /// pairs it wrote.
    ///
    /// Ask for the series on the market data request by the venue's own number
    /// for it: 434 and 548 are the two analyst ratings, and 454 is the insider
    /// and institutional interest, which is where a float and a share count
    /// are stated. The keys are the venue's own, unchanged.
    fn company_data(&self, con_id: u32, series: u32) -> PyResult<Vec<(String, String)>> {
        Ok(self
            .shared_state()
            .map(|s| s.reference.company_data(con_id, series))
            .unwrap_or_default())
    }

    /// Which of those series have been stated for a contract.
    fn company_data_series(&self, con_id: u32) -> PyResult<Vec<u32>> {
        Ok(self
            .shared_state()
            .map(|s| s.reference.company_data_series(con_id))
            .unwrap_or_default())
    }

    // ── Option calculations ──
    //
    // A volatility inverted from a price, and a price implied by a volatility.
    // This protocol carries no request for either: nothing it sends takes a
    // caller-supplied option price or volatility for the venue to work back
    // from. The calls are kept because a caller written against the reference
    // client calls them, and a call that reports why it cannot be served is
    // worth more than a missing attribute; they are not kept because they
    // might start working.

    /// What volatility a price implies for an option, under the model
    /// the venue publishes for that contract. Answered on
    /// `tick_option_computation`.
    ///
    /// `implied_vol_options` is taken and not applied. This protocol's request
    /// carries no free-form option list, so what a caller puts in one cannot be
    /// sent. The reference client's own list is empty on every ordinary call.
    #[pyo3(signature = (req_id, contract, option_price, under_price, implied_vol_options=Vec::new()))]
    fn calculate_implied_volatility(
        &self, py: Python<'_>, req_id: i64, contract: &Contract, option_price: f64,
        under_price: f64, implied_vol_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        let _ = implied_vol_options;
        if let Err(why) = self.answer_option_model(req_id, contract, |terms, model, schedule| {
            crate::control::option_model::implied_volatility(
                terms, model, schedule, option_price, under_price,
            )
        }, |volatility| crate::types::OptionComputation {
            implied_vol: volatility,
            opt_price: option_price,
            und_price: under_price,
            ..crate::types::OptionComputation::solved(req_id)
        }) {
            // The venue states a model for a contract that is watched. Asking
            // about one nobody is watching opens the watch and answers when
            // the model arrives, which is what the caller asked for — rather
            // than refusing the question for having been asked first.
            //
            // Only where that is the trouble. A model already stated and a
            // question it cannot answer is not something waiting will fix,
            // and kept anyway the caller was given neither an answer nor a
            // reason and waited on a model that had already arrived.
            let worth_waiting = why.message == crate::client_core::OPTION_MODEL_UNSTATED;
            if !worth_waiting || !self.watch_for_option_model(
                py, req_id, contract, true, option_price, under_price,
            )? {
                report_reason(self, req_id, &why);
            }
        }
        Ok(())
    }

    /// What an option is worth at a stated volatility, under the same
    /// model. Answered on `tick_option_computation`.
    ///
    /// `opt_prc_options` is taken and not applied. This protocol's request
    /// carries no free-form option list, so what a caller puts in one cannot be
    /// sent. The reference client's own list is empty on every ordinary call.
    #[pyo3(signature = (req_id, contract, volatility, under_price, opt_prc_options=Vec::new()))]
    fn calculate_option_price(
        &self, py: Python<'_>, req_id: i64, contract: &Contract, volatility: f64,
        under_price: f64, opt_prc_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        let _ = opt_prc_options;
        if let Err(why) = self.answer_option_model(req_id, contract, |terms, model, schedule| {
            crate::control::option_model::option_price(
                terms, model, schedule, volatility, under_price,
            )
        }, |price| crate::types::OptionComputation {
            implied_vol: volatility,
            opt_price: price,
            und_price: under_price,
            ..crate::types::OptionComputation::solved(req_id)
        }) {
            // As above: the watch is opened where the model has not been
            // stated, and the answer follows it. Where it has, and the
            // question still cannot be answered, that is said.
            let worth_waiting = why.message == crate::client_core::OPTION_MODEL_UNSTATED;
            if !worth_waiting || !self.watch_for_option_model(
                py, req_id, contract, false, volatility, under_price,
            )? {
                report_reason(self, req_id, &why);
            }
        }
        Ok(())
    }

    /// Stop waiting on an implied-volatility request.
    ///
    /// A question answered in the call it was asked in leaves nothing to
    /// withdraw. One that opened a watch is holding a subscription the caller
    /// never asked for by name, and this is what releases it.
    fn cancel_calculate_implied_volatility(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        self.forget_option_calc(py, req_id);
        Ok(())
    }

    /// As for [`cancel_calculate_implied_volatility`](Self::cancel_calculate_implied_volatility).
    fn cancel_calculate_option_price(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        self.forget_option_calc(py, req_id);
        Ok(())
    }


    // ── News Bulletins ──

    /// Ask for the notices the venue broadcasts to everyone. Answered on
    /// `update_news_bulletin`.
    ///
    /// `all_msgs` asks for the day's bulletins as well as the ones still to
    /// come. Nothing is sent to the venue: it broadcasts these unasked and has
    /// been doing so since the session opened, so the day's are answered from
    /// what is queued. Asking only for what follows drops that queue, or the
    /// next poll opens with a bulletin published before the caller asked for
    /// any. What cannot be had either way is anything from before the session
    /// existed, because there is no request to ask for it with. The last
    /// [`NEWS_BULLETIN_LIMIT`](crate::bridge::NEWS_BULLETIN_LIMIT)
    /// are kept for a caller who has not asked yet.
    #[pyo3(signature = (all_msgs=true))]
    fn req_news_bulletins(&self, all_msgs: bool) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        if !all_msgs {
            let _ = self.shared_state()?.market.drain_news_bulletins();
        }
        self.core.subscribe_bulletins();
        Ok(())
    }

    /// Stop receiving broadcast notices.
    fn cancel_news_bulletins(&self) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        self.core.unsubscribe_bulletins();
        Ok(())
    }

    // ── Server Time ──
    //
    // The venue is never asked what time it is: nothing on this wire asks it.
    // The answer is worked out here — this machine's clock, shifted by what
    // the venue has stated about its own, on the logon it stamps and in the
    // clock it pushes afterwards. A session that has been told nothing is
    // shifted by nothing, so it answers this machine's clock, and there is no
    // state in which this question has no answer.
    /// Ask for the venue's own clock. Answered on `current_time`.
    ///
    /// Before a session exists this is reported on `error`, the way every
    /// request made before connecting is: an answer waits for a dispatch pass,
    /// and with no session there is nothing to make one.
    fn req_current_time(&self, py: Python<'_>) -> PyResult<()> {
        let Some(_connected) = self.tx_or_report(-1)? else { return Ok(()) };
        let seconds = self.venue_time_millis().div_euclid(1_000);
        self.deliver(py, "current_time", (seconds,))?;
        Ok(())
    }

    /// Ask for the venue's own clock in milliseconds. Answered on
    /// `current_time_in_millis`.
    ///
    /// The same clock `req_current_time` reports and worked out the same way.
    /// What differs is the precision kept: asking in seconds throws away the
    /// fraction this one keeps.
    ///
    /// Before a session exists this is reported on `error`, as
    /// `req_current_time` is.
    fn req_current_time_in_millis(&self, py: Python<'_>) -> PyResult<()> {
        let Some(_connected) = self.tx_or_report(-1)? else { return Ok(()) };
        let millis = self.venue_time_millis();
        self.deliver(py, "current_time_in_millis", (millis,))?;
        Ok(())
    }

    // ── FA (Financial Advisor) ──

    /// Ask the venue for a partition of the advisor's own configuration.
    ///
    /// The reference client names the partition by a number: its groups, its
    /// allocation profiles, its aliases. The venue names it by a word, so the
    /// number is turned into the word it stands for. A number that stands for
    /// nothing is refused rather than sent as an empty partition.
    ///
    /// The venue's answer reaches `receive_fa` under the same number the
    /// partition was asked for by.
    fn request_fa(&self, py: Python<'_>, fa_data_type: i32) -> PyResult<()> {
        let Some(partition) = advisor_partition(fa_data_type) else {
            return self.report_refusal(py, -1, crate::error_codes::Refusal::validation(
                format!("no advisor configuration is named by {fa_data_type}"),
            ));
        };
        let Some(tx) = self.tx_or_report(-1)? else { return Ok(()) };
        Self::send_control(py, &tx, ControlCommand::AdvisorConfig {
            // Nothing to carry back: the answer to a question about a
            // partition names the partition, not a request.
            req_id: -1,
            // Asking for it by name.
            command: 5,
            partition: partition.to_string(),
            fa_data_type,
            document: None,
        })
    }

    #[pyo3(signature = (req_id, fa_data_type, cxml))]
    /// Replace a partition of the advisor's configuration with the one given.
    ///
    /// `replace_fa_end` fires with `req_id` once the venue has taken it, and
    /// a venue that refuses states why on `error` under the same number.
    fn replace_fa(&self, py: Python<'_>, req_id: i64, fa_data_type: i32, cxml: &str) -> PyResult<()> {
        let Some(partition) = advisor_partition(fa_data_type) else {
            return self.report_refusal(py, req_id, crate::error_codes::Refusal::validation(
                format!("no advisor configuration is named by {fa_data_type}"),
            ));
        };
        let Some(tx) = self.tx_or_report(-1)? else { return Ok(()) };
        Self::send_control(py, &tx, ControlCommand::AdvisorConfig {
            req_id,
            // Replacing it with what is carried.
            command: 3,
            partition: partition.to_string(),
            fa_data_type,
            document: Some(cxml.to_string()),
        })
    }

    // ── Display Groups ──

    /// Ask which display groups exist. Answered on
    /// `display_group_list`.
    fn query_display_groups(&self, req_id: i64) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        self.core.query_display_groups(req_id);
        Ok(())
    }

    /// Watch what a display group is showing. Answered on
    /// `display_group_updated`.
    fn subscribe_to_group_events(&self, req_id: i64, group_id: i32) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        self.core.subscribe_to_group_events(req_id, group_id);
        Ok(())
    }

    /// Stop watching a display group.
    fn unsubscribe_from_group_events(&self, req_id: i64) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        self.core.unsubscribe_from_group_events(req_id);
        Ok(())
    }

    /// Tell a display group what to show.
    fn update_display_group(&self, req_id: i64, contract_info: &str) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        // The reference client answers a request it cannot serve on the error
        // callback and returns normally. Raising here would make a caller
        // written against it fall over on a request that merely came in the
        // wrong order.
        if let Err(reason) = self.core.update_display_group(req_id, contract_info) {
            report_reason(self, req_id, &Refusal::validation(reason));
        }
        Ok(())
    }

    // ── Smart Components ──

    /// Ask which venue each bit of a quote's exchange mask refers to.
    /// The venue states the map beside the quote, so a quote has to have been
    /// asked for first. Answered on `smart_components`.
    ///
    /// `bbo_exchange` is taken and not applied. The venue states one table of
    /// routing components at logon, for this session rather than per exchange,
    /// and that whole table is what comes back.
    fn req_smart_components(&self, py: Python<'_>, req_id: i64, bbo_exchange: &str) -> PyResult<()> {
        let _ = bbo_exchange;
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        let shared = self.shared_state()?;
        let sc = shared.reference.smart_components();
        // A list, which is what the reference client's decoder builds: the name
        // it gives the argument says "map" and the thing it passes is a list, so
        // a program written against it iterates the components and reads each
        // one's fields. Handed a dict keyed by bit number, that loop walked the
        // keys and asked an integer for `bitNumber`.
        let mut components = Vec::with_capacity(sc.len());
        for c in sc.iter() {
            components.push(Py::new(py, SmartComponentPy {
                bit_number: c.bit_number,
                exchange: c.exchange.clone(),
                exchange_letter: c.exchange_letter.clone(),
            })?);
        }
        let list = pyo3::types::PyList::new(py, components)?;
        self.deliver(py, "smart_components", (req_id, list.as_any()))?;
        Ok(())
    }

    // ── News Providers ──

    /// Ask which news providers this account may read. Answered on
    /// `news_providers`.
    fn req_news_providers(&self, py: Python<'_>) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        let shared = self.shared_state()?;
        let np = shared.reference.news_providers();
        let mut providers: Vec<Py<NewsProviderPy>> = Vec::with_capacity(np.len());
        for p in np.iter() {
            let obj = NewsProviderPy { code: p.code.clone(), name: p.name.clone() };
            providers.push(Py::new(py, obj)?);
        }
        let py_list = pyo3::types::PyList::new(py, providers)?;
        self.deliver(py, "news_providers", (py_list.as_any(),))?;
        Ok(())
    }

    // ── Soft Dollar Tiers ──

    /// Ask which soft dollar tiers this account may direct commission
    /// to. Answered on `soft_dollar_tiers`.
    fn req_soft_dollar_tiers(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        let shared = self.shared_state()?;
        let tiers = shared.reference.soft_dollar_tiers();
        let mut objs: Vec<Py<SoftDollarTierPy>> = Vec::with_capacity(tiers.len());
        for t in tiers.iter() {
            let obj = SoftDollarTierPy {
                name: t.name.clone(),
                val: t.val.clone(),
                display_name: t.display_name.clone(),
            };
            objs.push(Py::new(py, obj)?);
        }
        let py_list = pyo3::types::PyList::new(py, objs)?;
        self.deliver(py, "soft_dollar_tiers", (req_id, py_list.as_any()))?;
        Ok(())
    }

    // ── Family Codes ──

    /// Ask which account families this login belongs to. Answered on
    /// `family_codes`.
    fn req_family_codes(&self, py: Python<'_>) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        let shared = self.shared_state()?;
        let codes = shared.reference.family_codes();
        // Objects, as the reference client passes them: a program reads
        // `code.accountID`, which a pair does not answer to.
        let mut family = Vec::with_capacity(codes.len());
        for fc in codes.iter() {
            family.push(Py::new(py, crate::python::compat::class_reports::FamilyCodePy {
                account_id: fc.account_id.clone(),
                family_code_str: fc.family_code_str.clone(),
            })?);
        }
        let py_list = pyo3::types::PyList::new(py, family)?;
        self.deliver(py, "family_codes", (py_list.as_any(),))?;
        Ok(())
    }

    // ── Server Log Level ──

    /// How much to log about this session, 1 to 5.
    ///
    /// Recorded locally rather than sent: this wire carries no log-level
    /// request. A level outside 1 to 5 is refused rather than reported back as
    /// `warn`, which would tell a caller they had a level that does not
    /// exist.
    #[pyo3(signature = (log_level=2))]
    fn set_server_log_level(&self, py: Python<'_>, log_level: i32) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        let level = match log_level {
            1 => "error",
            2 => "warn",
            3 => "info",
            4 => "debug",
            5 => "trace",
            _ => return self.report_refusal(py, -1, crate::error_codes::Refusal::stated(
                crate::error_codes::LOG_LEVEL_INVALID,
                format!("set_server_log_level: {log_level} is not a log level; it is 1 to 5"),
            )),
        };
        // The logger this client installed, as on the other surface. Nothing
        // goes to the venue: this protocol carries no message asking one to
        // change how loudly it talks, and a level a caller states is about the
        // thing serving that caller — which, in a library, is this.
        if crate::logging::set_level(level) {
            log::info!("set_server_log_level: logging at {level} (level {log_level})");
            return Ok(());
        }
        // A program that installed its own logger keeps it, and saying the
        // level moved when it did not is worse than saying it did not.
        self.report_refusal(py, -1, crate::error_codes::Refusal::stated(
            crate::error_codes::LOG_LEVEL_INVALID,
            format!(
                "set_server_log_level: {level} was not applied because this session did \
                 not install the logger; whoever did holds the level"
            ),
        ))
    }

    // ── User Info ──

    /// Ask what this login is entitled to. Answered on `user_info`.
    fn req_user_info(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        let shared = self.shared_state()?;
        let id = shared.reference.white_branding_id();
        self.deliver(py, "user_info", (req_id, id))?;
        Ok(())
    }

    // ── WSH ──

    /// What event types the corporate-events calendar carries. Answered on
    /// `wshMetaData`.
    fn req_wsh_meta_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        Self::send_control(py, &tx, ControlCommand::FetchCalendarMetaData {
            req_id: wire_req_id(req_id)?,
        })
    }

    /// Stop waiting on the event types.
    ///
    /// The query is one message and one answer, so there is nothing at the
    /// venue to withdraw: what is withdrawn is the answer, which would
    /// otherwise reach a caller who has said they are done with it. A cancel
    /// naming no waiting request says so rather than returning as though it
    /// acted.
    fn cancel_wsh_meta_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        Self::send_control(py, &tx, ControlCommand::CancelCalendar {
            req_id: wire_req_id(req_id)?,
        })
    }

    /// Stop waiting on the calendar's events. As above.
    fn cancel_wsh_event_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        Self::send_control(py, &tx, ControlCommand::CancelCalendar {
            req_id: wire_req_id(req_id)?,
        })
    }

    /// The calendar's events. Answered on `wshEventData`.
    ///
    /// `wsh_event_data` is the object the public API takes: a contract id, or
    /// a filter the caller writes, plus the window and what to fill from.
    #[pyo3(signature = (req_id, wsh_event_data=None))]
    fn req_wsh_event_data(&self, py: Python<'_>, req_id: i64, wsh_event_data: Option<Py<PyAny>>) -> PyResult<()> {
        let mut query = crate::types::CalendarQuery::default();
        if let Some(asked) = wsh_event_data.as_ref() {
            let asked = asked.bind(py);
            let text = |name: &str| -> String {
                asked
                    .getattr(name)
                    .ok()
                    .and_then(|v| v.extract::<String>().ok())
                    .unwrap_or_default()
            };
            let flag = |name: &str| -> bool {
                asked.getattr(name).ok().and_then(|v| v.extract::<bool>().ok()).unwrap_or(false)
            };
            // The number the reference client leaves in a field nobody set.
            // It reaches here as an ordinary integer, so a request built the
            // way that client builds one — construct the object, set the
            // filter, send it — arrives naming contract 2147483647 and asking
            // for that many rows. Both are the caller saying nothing.
            const UNSET: i64 = i32::MAX as i64;
            let con_id = asked
                .getattr("conId")
                .ok()
                .and_then(|v| v.extract::<i64>().ok())
                .filter(|id| *id > 0 && *id != UNSET);
            query.con_id = con_id;
            query.filter = text("filter");
            query.start_date = text("startDate");
            query.end_date = text("endDate");
            query.fill_watchlist = flag("fillWatchlist");
            query.fill_portfolio = flag("fillPortfolio");
            query.fill_competitors = flag("fillCompetitors");
            query.total_limit = asked
                .getattr("totalLimit")
                .ok()
                .and_then(|v| v.extract::<i64>().ok())
                .filter(|n| *n > 0 && *n < i64::MAX && *n != UNSET);
        }
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        Self::send_control(py, &tx, ControlCommand::FetchCalendarEvents {
            req_id: wire_req_id(req_id)?,
            query: Box::new(query),
        })
    }
}

impl EClient {
    /// The venue's clock in milliseconds: this machine's, shifted by what the
    /// venue has stated about its own.
    ///
    /// The venue is never asked for it, so there is no state in which this
    /// cannot be answered. A client with nothing behind it has been told
    /// nothing, and nothing shifted by nothing is this machine's clock, which
    /// is the answer before the venue has stated anything at all.
    fn venue_time_millis(&self) -> i64 {
        match self.shared_state() {
            Ok(shared) => shared.market.venue_time_millis(),
            Err(_) => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as i64),
        }
    }
}

/// Solve an option against the venue's published model.
///
/// The wire carries no request for either calculation, so both are solved
/// locally. The answer is reported on `tick_option_computation`, the same
/// callback tick type 13 arrives on.
impl EClient {
    /// Open a watch on the contract and keep the question until the venue
    /// states a model for it. Answers whether the question is now kept.
    ///
    /// The watch is held under the caller's own request, whoever else is
    /// watching the contract: a question that opened none of its own is
    /// answerable only while somebody else keeps theirs up, and the moment
    /// they withdraw it there is no model coming and nothing said about it.
    fn watch_for_option_model(
        &self, py: Python<'_>, req_id: i64, contract: &Contract,
        wants_volatility: bool, option_price: f64, under_price: f64,
    ) -> PyResult<bool> {
        // A subscription this client opens rather than the caller. Refusals
        // are reported by the subscribe itself and leave nothing watching,
        // which is what is read back here rather than the call's own result:
        // this surface answers a refusal on the error callback and returns
        // normally, so the result alone does not say whether it took.
        //
        // What is read back is the slot the request holds. Not one found under
        // the contract's conId alone: a contract stated by description carries
        // none, and the engine is the first to know which slot it resolved to
        // — so asked that way the answer was no however the subscribe went, and
        // every question about a described contract was refused with the watch
        // it had just opened left running.
        //
        // A request already holding a slot is the case to be careful with. It
        // may be watching this very contract, which is what makes the venue
        // state a model at all — and it cannot open a second watch, so the
        // subscribe below would be refused and the question turned down on a
        // contract that is already being watched. Where the contract names an
        // id, its slot answers which of the two this is; where it does not,
        // there is nothing to compare and the caller's own watch is not this
        // question's to claim.
        let Ok(shared) = self.shared_state() else { return Ok(false) };
        let its_own = contract.to_api().con_id;
        let its_slot = || {
            (its_own != 0)
                .then(|| self.core.cached_instrument(&shared, its_own))
                .flatten()
        };
        // The request's own slot, where it already has one, may be this
        // contract's — which is what makes the venue state a model at all, and
        // a request already watching something is refused a second watch. Asked
        // only whether it holds a slot, such a request read as watching some
        // other contract and the question was turned down on one the venue was
        // already stating a model for.
        if !(its_slot().is_some() && its_slot() == self.core.watching(req_id)) {
            // Asked for even where the slot it holds is another contract's,
            // because the refusal that answers is the caller's to hear — and an
            // interrupt raised in their handler for it leaves the call rather
            // than being swallowed here.
            let held_before = self.core.holds_mkt_data(req_id);
            self.req_mkt_data(py, req_id, contract, "", false, false, Vec::new())?;
            let took = if its_own != 0 {
                its_slot().is_some() && its_slot() == self.core.watching(req_id)
            } else {
                // A contract stated by description carries no id to compare, and
                // the engine is the first to know which slot it resolved to. What
                // it held before still has to be asked: a request already watching
                // something is refused a second watch, so a slot found after a
                // refused subscribe is the other contract's — kept, the question
                // was answered on it and cancelling the question withdrew it.
                !held_before && self.core.holds_mkt_data(req_id)
            };
            if !took {
                return Ok(false);
            }
        }
        self.pending_option_calcs.lock().unwrap().insert(
            req_id,
            crate::api::client::PendingOptionCalc {
                contract: contract.to_api(),
                wants_volatility,
                option_price,
                under_price,
                answered: false,
            },
        );
        Ok(true)
    }

    /// Drop a kept question, and the watch it opened.
    ///
    /// Only its own hold goes: a contract another question is still watching
    /// keeps its subscription, which passes to whoever is left. Held back here
    /// instead, on questions naming the same contract, two contracts stated by
    /// description read as one — neither carries a conId to tell them apart —
    /// and the withdrawal of the first was skipped for a second that was
    /// watching something else entirely.
    fn forget_option_calc(&self, py: Python<'_>, req_id: i64) {
        if self.pending_option_calcs.lock().unwrap().remove(&req_id).is_none() {
            return;
        }
        // The watch was this client's own, so it goes without a word: the
        // caller withdrew a question, not a subscription.
        if let Ok(tx) = self.tx() {
            let _ = self.withdraw_mkt_data(py, &tx, req_id);
        }
    }

    /// Answer the questions that were waiting on the venue to state a model,
    /// and forget them.
    ///
    /// One the venue still cannot answer is kept: the watch is open, so the
    /// model may yet arrive. It is dropped when the caller withdraws it.
    pub(crate) fn answer_kept_option_calcs(&self) {
        let kept: Vec<(i64, crate::api::client::PendingOptionCalc)> = self
            .pending_option_calcs.lock().unwrap()
            .iter().map(|(k, v)| (*k, v.clone())).collect();
        for (req_id, calc) in kept {
            if calc.answered {
                continue;
            }
            if self.solve_and_push_kept(req_id, &calc) {
                // Marked, not dropped, so the caller's withdrawal still has a
                // question to find and can still take down the watch this
                // client opened to obtain the model. Nothing is sent here.
                if let Some(kept) = self.pending_option_calcs.lock().unwrap().get_mut(&req_id) {
                    kept.answered = true;
                }
            }
        }
    }

    /// Answer one kept question, if the venue has stated a model by now.
    /// Answers whether it did.
    fn solve_and_push_kept(
        &self, req_id: i64, calc: &crate::api::client::PendingOptionCalc,
    ) -> bool {
        let Ok(shared) = self.shared_state() else { return false };
        let (given, und) = (calc.option_price, calc.under_price);
        let wants_volatility = calc.wants_volatility;
        let solved = self.core.solve_option(&shared, &calc.contract, Some(req_id), |terms, model, schedule| {
            if wants_volatility {
                crate::control::option_model::implied_volatility(
                    terms, model, schedule, given, und,
                )
            } else {
                crate::control::option_model::option_price(terms, model, schedule, given, und)
            }
        });
        match solved {
            Ok(answer) => {
                // The caller supplied one of the pair and asked for the other,
                // so the answer takes the side they left open.
                let (implied_vol, opt_price) =
                    if wants_volatility { (answer, given) } else { (given, answer) };
                shared.market.push_option_computation(crate::types::OptionComputation {
                    implied_vol,
                    opt_price,
                    und_price: und,
                    ..crate::types::OptionComputation::solved(req_id)
                });
                true
            }
            // As on the other surface: only the refusal saying the venue has
            // not stated its model resolves by waiting. The rest never do, and
            // read as "not yet" they keep the question for the life of the
            // session with nothing ever said about it.
            Err(why) if why.message == crate::client_core::OPTION_MODEL_UNSTATED => false,
            Err(why) => {
                report_reason(self, req_id, &why);
                true
            }
        }
    }

    /// Work an option-model answer out of what the venue has stated.
    ///
    /// `req_id` reaches nothing here: this states the answer and the caller
    /// above it is what carries the number, so naming it twice would let the
    /// two disagree. Nor does it name the request the model is watched for —
    /// this is a first ask, which has opened no watch yet, and the number is
    /// the caller's own: it may already be watching a contract of its
    /// choosing, and for one with no id of its own that other contract's slot
    /// is what a fall-back to it would find.
    fn answer_option_model(
        &self,
        _req_id: i64,
        contract: &Contract,
        solve: impl Fn(
            crate::control::option_model::OptionTerms,
            crate::control::option_model::VenueModel,
            &[(f64, f64)],
        ) -> Option<f64>,
        into_computation: impl Fn(f64) -> crate::types::OptionComputation,
    ) -> Result<(), Refusal> {
        // The refusal is carried whole. Flattened to its text the code went
        // with it, and every one of them reached a caller as the same number —
        // which is the one thing a caller written against the reference client
        // branches on.
        let shared = self.shared_state()
            .map_err(|_| Refusal::not_connected("not connected"))?;
        let answer = self.core.solve_option(&shared, &contract.to_api(), None, solve)?;
        shared.market.push_option_computation(into_computation(answer));
        Ok(())
    }
}

/// Answer a request this client cannot serve the way the reference client
/// does: on the error callback, returning normally.
///
/// Takes the code for the specific refusal rather than the general one.
pub(crate) fn report_unserviceable_with(
    client: &EClient, req_id: i64, code: i32, reason: &str,
) {
    if let Ok(shared) = client.shared_state() {
        shared.reference.push_historical_error(carried_under(req_id), code, reason.to_string());
    }
}

/// Answer a request this client cannot serve the way the reference client
/// does: on the error callback, returning normally.
///
/// The refusal's own code is carried rather than one number for all of them,
/// and a refusal belonging to no request keeps that rather than being clamped
/// onto request zero, which a caller may well have asked under.
fn report_reason(client: &EClient, req_id: i64, reason: &Refusal) {
    if let Ok(shared) = client.shared_state() {
        shared.reference.push_historical_error(
            carried_under(req_id), reason.code, reason.message.clone(),
        );
    }
}

/// The request a refusal is reported against, or the mark for none.
///
/// The same rule the Rust surface keeps, and for the same reason: a number too
/// wide to carry is reported against no request rather than against its own
/// low half.
fn carried_under(req_id: i64) -> u32 {
    crate::api::client::carried_under(req_id)
}

/// The word the venue names a partition of an advisor's configuration by.
///
/// The reference client names it by a number. The two vocabularies are not the
/// same, and sending the number would ask for a partition that does not exist.
///
/// The numbers are the reference client's, and they run groups, profiles,
/// aliases — which is what this surface's own reference states and what the
/// Rust surface sends. Rotated by one here, a caller that asked for its groups
/// was given its aliases.
fn advisor_partition(fa_data_type: i32) -> Option<&'static str> {
    match fa_data_type {
        1 => Some("Group"),
        2 => Some("Profile"),
        3 => Some("Aliases"),
        _ => None,
    }
}

#[cfg(test)]
mod advisor_partition_tests {
    use super::advisor_partition;

    /// The reference client names a partition of an advisor's configuration by
    /// a number; the venue names it by a word. Sending the number would ask for
    /// a partition that does not exist.
    #[test]
    fn a_number_is_turned_into_the_word_the_venue_uses() {
        // The order is the reference client's: groups, profiles, aliases.
        assert_eq!(advisor_partition(1), Some("Group"));
        assert_eq!(advisor_partition(2), Some("Profile"));
        assert_eq!(advisor_partition(3), Some("Aliases"));
    }

    /// A number standing for nothing is refused rather than sent as an empty
    /// partition, which the venue would answer for something else or not at all.
    #[test]
    fn a_number_standing_for_nothing_names_nothing() {
        for unknown in [0, 4, -1, 99] {
            assert_eq!(advisor_partition(unknown), None, "{unknown}");
        }
    }
}

#[cfg(test)]
mod option_model_watch_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc::Receiver;
    use std::sync::atomic::Ordering;
    use crate::bridge::SharedState;

    /// A connected client whose engine is a channel the test reads, and the
    /// wrapper it reports to.
    fn wired(py: Python<'_>) -> (EClient, Receiver<ControlCommand>, Py<PyAny>) {
        let client = EClient::__new__(&pyo3::types::PyTuple::empty(py), None);
        let wrapper = py
            .eval(c"__import__('builtins').type('W', (), {'__init__': lambda s: setattr(s, 'calls', []), '__getattr__': lambda s, n: (lambda *a: s.calls.append((n, a)))})()", None, None)
            .unwrap()
            .unbind();
        client.__init__(wrapper.clone_ref(py)).unwrap();
        let shared = Arc::new(SharedState::new());
        shared.market.set_instrument_count(2);
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        *client.shared.lock().unwrap() = Some(shared);
        *client.control_tx.lock().unwrap() = Some(tx);
        client.connected.store(true, Ordering::Release);
        // A registration answered from another thread has to outlast being
        // scheduled; see the same note beside the client this file's
        // neighbours build.
        client.core.set_registration_timeout(std::time::Duration::from_secs(30));
        (client, rx, wrapper)
    }

    /// An engine that answers the next subscribe with the slot it resolved
    /// the description to, which is the only side that knows it, and hands the
    /// channel back.
    fn resolves_to(
        rx: Receiver<ControlCommand>, slot: u32,
    ) -> std::thread::JoinHandle<Receiver<ControlCommand>> {
        std::thread::spawn(move || {
            while let Ok(cmd) = rx.recv() {
                if let ControlCommand::Subscribe { reply_tx: Some(reply), .. } = cmd {
                    let _ = reply.send(Ok(slot));
                    return rx;
                }
            }
            panic!("the watch must reach the engine");
        })
    }

    /// An option the caller states by description, carrying no conId.
    fn described(symbol: &str) -> Contract {
        Contract {
            symbol: symbol.into(), sec_type: "OPT".into(), exchange: "SMART".into(),
            currency: "USD".into(), last_trade_date_or_contract_month: "20261218".into(),
            strike: 100.0, right: "C".into(), multiplier: "100".into(), ..Default::default()
        }
    }

    /// A question about a contract the caller described is kept.
    ///
    /// The venue states a model only for a contract that is watched, so the
    /// question opens a watch and waits. What says the watch took is the slot
    /// the request now holds: a described contract carries no conId, and the
    /// engine is the first to know which slot it resolved to. Asked for a
    /// conId the answer was no however the subscribe went — the question was
    /// refused every time, with the watch it had just opened left running.
    #[test]
    fn a_question_about_a_described_contract_is_kept_against_the_slot_the_request_took() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, wrapper) = wired(py);
            let engine = resolves_to(rx, 0);
            client.calculate_implied_volatility(py, 7, &described("SPY"), 1.0, 100.0, Vec::new())
                .unwrap();
            let _rx = py.detach(|| engine.join().unwrap());

            assert!(client.pending_option_calcs.lock().unwrap().contains_key(&7),
                "the question waits on the model the watch will bring");
            assert_eq!(client.core.watching(7), Some(0), "under the slot the engine resolved");
            let told: Vec<String> = wrapper.getattr(py, "calls").unwrap()
                .cast_bound::<pyo3::types::PyList>(py).unwrap().iter()
                .map(|c| c.get_item(0).unwrap().extract::<String>().unwrap())
                .collect();
            assert!(told.is_empty(), "and the caller is told nothing while it waits: {told:?}");
        });
    }

    /// Withdrawing one question takes down its own watch, whatever else is
    /// waiting. Two contracts stated by description carry the same nought
    /// where a conId would be, so a withdrawal held back for a question naming
    /// "the same contract" was held back for one watching something else, and
    /// the subscription ran for the rest of the session with nothing left to
    /// withdraw it.
    #[test]
    fn withdrawing_one_described_question_withdraws_its_own_watch() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, _wrapper) = wired(py);
            let engine = resolves_to(rx, 0);
            client.calculate_implied_volatility(py, 7, &described("SPY"), 1.0, 100.0, Vec::new())
                .unwrap();
            let rx = py.detach(|| engine.join().unwrap());
            let engine = resolves_to(rx, 1);
            client.calculate_implied_volatility(py, 8, &described("QQQ"), 1.0, 100.0, Vec::new())
                .unwrap();
            let rx = py.detach(|| engine.join().unwrap());

            client.cancel_calculate_implied_volatility(py, 7).unwrap();
            let withdrawn: Vec<u32> = rx.try_iter()
                .filter_map(|cmd| match cmd {
                    ControlCommand::Unsubscribe { instrument, .. } => Some(instrument),
                    _ => None,
                })
                .collect();
            assert_eq!(withdrawn, [0], "the withdrawn question's own slot, and only it");
            assert!(client.pending_option_calcs.lock().unwrap().contains_key(&8),
                "the question still waiting keeps its watch");
        });
    }

    /// Each question holds the shared watch under its own number. Withdrawing
    /// either first passes the subscription to the question that remains.
    #[test]
    fn shared_option_questions_release_the_watch_in_either_cancel_order() {
        Python::initialize();
        Python::attach(|py| {
            for ids in [[7, 8], [8, 7]] {
                let (client, rx, _wrapper) = wired(py);
                client.core.set_registration_timeout(std::time::Duration::from_secs(5));
                let option = Contract { con_id: 1234, ..described("SPY") };
                let engine = resolves_to(rx, 0);
                client.calculate_implied_volatility(py, 7, &option, 1.0, 100.0, Vec::new()).unwrap();
                let rx = py.detach(|| engine.join().unwrap());
                client.calculate_option_price(py, 8, &option, 0.2, 100.0, Vec::new()).unwrap();
                assert_eq!(client.core.watching(7), Some(0));
                assert_eq!(client.core.watching(8), Some(0));
                assert!(!rx.try_iter().any(|cmd| matches!(cmd, ControlCommand::Subscribe { .. })),
                    "both questions share one wire subscription");
                let cancel = |id| {
                    if id == 7 { client.cancel_calculate_implied_volatility(py, id).unwrap(); }
                    else { client.cancel_calculate_option_price(py, id).unwrap(); }
                };
                cancel(ids[0]);
                assert_eq!(client.core.watching(ids[0]), None);
                assert_eq!(client.core.watching(ids[1]), Some(0));
                assert!(!rx.try_iter().any(|cmd| matches!(cmd, ControlCommand::Unsubscribe { .. })));
                cancel(ids[1]);
                let withdrawn: Vec<_> = rx.try_iter().filter_map(|cmd| match cmd {
                    ControlCommand::Unsubscribe { instrument, .. } => Some(instrument),
                    _ => None,
                }).collect();
                assert_eq!(withdrawn, [0]);
                assert!(client.pending_option_calcs.lock().unwrap().is_empty());
                assert_eq!(client.core.watching(ids[1]), None);
            }
        });
    }
}

#[cfg(test)]
mod calendar_request_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use crate::bridge::SharedState;

    /// A field the caller never set names no contract and caps nothing.
    ///
    /// The reference client fills an unset whole number with the largest one
    /// there is, and a caller builds the object, writes the one field it cares
    /// about and sends it. Read as stated, the request asked the calendar
    /// about contract 2147483647 — which is nobody's — instead of the portfolio
    /// or competitors it was scoped by, and asked for that many rows.
    #[test]
    fn a_calendar_field_the_caller_never_set_states_nothing() {
        Python::initialize();
        Python::attach(|py| {
            let client = EClient::__new__(&pyo3::types::PyTuple::empty(py), None);
            let (tx, rx) = std::sync::mpsc::sync_channel(4);
            *client.shared.lock().unwrap() = Some(Arc::new(SharedState::new()));
            *client.control_tx.lock().unwrap() = Some(tx);
            client.connected.store(true, Ordering::Release);

            // The object as that client leaves it: every whole number unset,
            // one field written.
            let asked = py
                .eval(
                    c"__import__('builtins').type('W', (), {})()",
                    None, None,
                )
                .unwrap();
            asked.setattr("conId", i32::MAX).unwrap();
            asked.setattr("totalLimit", i32::MAX).unwrap();
            asked.setattr("filter", "").unwrap();

            client.req_wsh_event_data(py, 11, Some(asked.unbind())).unwrap();

            let Some(ControlCommand::FetchCalendarEvents { query, .. }) = rx.try_iter().next()
            else {
                panic!("the request reaches the engine");
            };
            assert_eq!(query.con_id, None, "no contract was named");
            assert_eq!(query.total_limit, None, "no number of rows was asked for");
        });
    }
}
