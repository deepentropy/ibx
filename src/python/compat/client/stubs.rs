//! Gateway-local fakes and pure no-op stubs.

use pyo3::prelude::*;

use super::EClient;
use super::super::contract::{Contract, NewsProviderPy, SoftDollarTierPy, WshEventDataPy};

impl EClient {
    /// The smart_components callback, or the error of the request.
    pub(crate) fn deliver_smart_components(
        &self,
        py: Python<'_>,
        req_id: i64,
        answer: crate::client_core::SmartComponentsAnswer,
    ) -> PyResult<()> {
        let sc = match answer {
            Ok(sc) => sc,
            Err((code, msg)) => {
                self.cb(py).call_method1(py, "error", (req_id, code, msg.as_str(), ""))?;
                return Ok(());
            }
        };
        // The map of the official client library: bit number to the pair
        // (exchange, exchange letter) (ibx#524).
        let map = pyo3::types::PyDict::new(py);
        for c in sc.iter() {
            map.set_item(c.bit_number, (c.exchange.as_str(), c.exchange_letter.as_str()))?;
        }
        self.cb(py).call_method1(py, "smart_components", (req_id, map.as_any()))?;
        Ok(())
    }

    /// The error of a refused display group request (ibx#424).
    fn display_group_refused(&self, py: Python<'_>, text: &str) -> PyResult<()> {
        let (id, code) = crate::client_core::DISPLAY_GROUP_REFUSAL;
        self.cb(py).call_method1(py, "error", (id, code, text, ""))?;
        Ok(())
    }

    fn calculate_option(
        &self, py: Python<'_>, req_id: i64, contract: &Contract,
        kind: crate::control::optcalc::CalcKind, under_price: f64,
    ) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("calculate_option", &[req_id, contract.con_id]) { return Ok(()); }
        let shared = self.shared_state()?;
        let api = contract.to_api();
        let price = matches!(kind, crate::control::optcalc::CalcKind::Price { .. });
        let features = shared.reference.account_features();
        if let Some((code, text)) = crate::client_core::ClientCore::option_calc_refusal(&api, price, features.as_deref()) {
            shared.orders.push_order_error(req_id, code, text);
            return Ok(());
        }
        let tx = self.tx()?;
        let request = crate::types::ControlCommand::CalcOption {
            req_id, con_id: contract.con_id, exchange: api.exchange.clone(), kind, under_price,
        };
        super::send_cmd(py, &tx, crate::client_core::ClientCore::resolve_first(req_id, &api, request))?;
        Ok(())
    }
}

#[pymethods]
impl EClient {
    // ── Options Calculations ──

    /// Implied volatility of an option price, computed locally by the
    /// option model as the reference (ibx#442). The options are not used.
    #[pyo3(signature = (req_id, contract, option_price, under_price, implied_vol_options=Vec::new()))]
    fn calculate_implied_volatility(
        &self, py: Python<'_>, req_id: i64, contract: &Contract, option_price: f64,
        under_price: f64, implied_vol_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return Ok(()); }
        let _ = implied_vol_options;
        self.calculate_option(py, req_id, contract, crate::control::optcalc::CalcKind::ImpliedVol { option_price }, under_price)
    }

    /// Price and greeks of an option at a volatility, computed locally by
    /// the option model as the reference (ibx#442). The options are not used.
    #[pyo3(signature = (req_id, contract, volatility, under_price, opt_prc_options=Vec::new()))]
    fn calculate_option_price(
        &self, py: Python<'_>, req_id: i64, contract: &Contract, volatility: f64,
        under_price: f64, opt_prc_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return Ok(()); }
        let _ = opt_prc_options;
        self.calculate_option(py, req_id, contract, crate::control::optcalc::CalcKind::Price { volatility }, under_price)
    }

    fn cancel_calculate_implied_volatility(&self, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        let _ = req_id;
        Ok(())
    }

    fn cancel_calculate_option_price(&self, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        let _ = req_id;
        Ok(())
    }

    #[pyo3(signature = (req_id, contract, exercise_action, exercise_quantity, account, _override))]
    fn exercise_options(
        &self, req_id: i64, contract: &Contract, exercise_action: i32,
        exercise_quantity: i32, account: &str, _override: i32,
    ) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return Ok(()); }
        let _ = (req_id, contract, exercise_action, exercise_quantity, account, _override);
        log::warn!("exercise_options: not yet implemented in engine");
        Ok(())
    }


    // ── News Bulletins ──

    #[pyo3(signature = (all_msgs=true))]
    fn req_news_bulletins(&self, all_msgs: bool) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.subscribe_bulletins(all_msgs);
        Ok(())
    }

    fn cancel_news_bulletins(&self) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.unsubscribe_bulletins();
        Ok(())
    }

    // ── Server Time ──

    fn req_current_time(&self, py: Python<'_>) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        // Paced like every request: on the reference a current time
        // request made in a burst was answered at its turn (run of
        // 10/10/2026, ibx#561).
        if self.later(move |this, py| this.req_current_time(py)) { return Ok(()); }
        // The local clock plus the offset to the server clock of the
        // logon, as the reference (ibx#421).
        let now = self.shared_state()?.reference.server_time_secs();
        self.cb(py).call_method1(py, "current_time", (now,))?;
        Ok(())
    }

    fn req_current_time_in_millis(&self, py: Python<'_>) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if self.later(move |this, py| this.req_current_time_in_millis(py)) { return Ok(()); }
        // The clock of req_current_time, in milliseconds (ibx#516).
        let now = self.shared_state()?.reference.server_time_millis();
        self.cb(py).call_method1(py, "current_time_in_millis", (now,))?;
        Ok(())
    }

    // ── Requests a session of the reference does not have (ibx#516) ──

    /// Below session level 215, error 503 for the request, as the official
    /// client library answers; from 215 nothing (ibx#557). A running
    /// contract details request goes on in both cases.
    fn cancel_contract_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        // The official client library's own check, on the caller's thread
        // before anything is sent: never put off (ibx#532).
        if let Some((code, text)) = crate::client_core::lookup_cancel_refusal(crate::client_core::session_level(), false) {
            self.cb(py).call_method1(py, "error", (req_id, code, text, ""))?;
        }
        Ok(())
    }

    /// Below session level 215, error 503 for the request; from 215
    /// nothing (ibx#557). A running historical ticks request goes on.
    fn cancel_historical_ticks(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        // The official client library's own check, on the caller's thread
        // before anything is sent: never put off (ibx#532).
        if let Some((code, text)) = crate::client_core::lookup_cancel_refusal(crate::client_core::session_level(), true) {
            self.cb(py).call_method1(py, "error", (req_id, code, text, ""))?;
        }
        Ok(())
    }

    /// A session opened without the intent to verify: error 544.
    #[pyo3(signature = (api_name, api_version))]
    fn verify_request(&self, py: Python<'_>, api_name: &str, api_version: &str) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        let _ = (api_name, api_version);
        if let Some(r) = self.not_connected(-1) { return r; }
        // With the intent to verify: sent, and the session answers no more
        // request, as the reference 10.51 (ibx#556).
        if self.core.extra_auth() {
            self.core.verify_sent();
            return Ok(());
        }
        let (name, version) = (api_name.to_string(), api_version.to_string());
        if self.later(move |this, py| this.verify_request(py, &name, &version)) { return Ok(()); }
        let (id, code, text) = crate::client_core::VERIFY_REQUEST_NO_INTENT;
        self.cb(py).call_method1(py, "error", (id, code, text, ""))?;
        Ok(())
    }

    /// No verification is in progress: no answer, as the reference 10.51
    /// (ibx#556; paper, 17/10/2026, a client at level 214); error 10095 as
    /// the reference 10.40 in its replays.
    #[pyo3(signature = (api_data))]
    fn verify_message(&self, py: Python<'_>, api_data: &str) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        let _ = api_data;
        if let Some(r) = self.not_connected(-1) { return r; }
        let data = api_data.to_string();
        if self.later(move |this, py| this.verify_message(py, &data)) { return Ok(()); }
        if self.shared_state()?.reference.form_10_40() {
            let (id, code, text) = crate::client_core::VERIFY_MESSAGE_OUT_OF_SEQUENCE;
            self.cb(py).call_method1(py, "error", (id, code, text, ""))?;
            return Ok(());
        }
        // No answer, and the session answers no more request, as the
        // reference 10.51 (ibx#556).
        self.core.verify_sent();
        Ok(())
    }

    /// A session opened without the intent to verify: error 551.
    #[pyo3(signature = (api_name, api_version, opaque_isv_key))]
    fn verify_and_auth_request(&self, py: Python<'_>, api_name: &str, api_version: &str, opaque_isv_key: &str) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        let _ = (api_name, api_version, opaque_isv_key);
        if let Some(r) = self.not_connected(-1) { return r; }
        if self.core.extra_auth() {
            self.core.verify_sent();
            return Ok(());
        }
        let (name, version, key) = (api_name.to_string(), api_version.to_string(), opaque_isv_key.to_string());
        if self.later(move |this, py| this.verify_and_auth_request(py, &name, &version, &key)) { return Ok(()); }
        let (id, code, text) = crate::client_core::VERIFY_AND_AUTH_REQUEST_NO_INTENT;
        self.cb(py).call_method1(py, "error", (id, code, text, ""))?;
        Ok(())
    }

    /// The reference gives no answer: nothing happens.
    #[pyo3(signature = (api_data, xyz_response))]
    fn verify_and_auth_message(&self, api_data: &str, xyz_response: &str) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        let _ = (api_data, xyz_response);
        if let Some(r) = self.not_connected(-1) { return r; }
        // The session answers no more request, as the reference 10.51
        // (ibx#556).
        if !self.shared_state()?.reference.form_10_40() {
            self.core.verify_sent();
        }
        Ok(())
    }

    // ── Configuration ──

    /// Request the configuration, as the official `reqConfigProtoBuf`
    /// (session level 219): a `ConfigRequest` of the official library, or
    /// an object with its attributes; None does nothing. The answer is
    /// `config_response_proto_buf`: the paper installation's configuration
    /// with the changes kept from `update_config_proto_buf` (ibx#610).
    /// Below level 219, error 503 for the request, as the official client
    /// library answers, and nothing sent. From 219 a logon that forbids
    /// configuration access gets 321.
    #[pyo3(signature = (config_request_proto))]
    fn req_config_proto_buf(&self, py: Python<'_>, config_request_proto: Option<Bound<'_, PyAny>>) -> PyResult<()> {
        let Some(proto) = config_request_proto.filter(|p| !p.is_none()) else { return Ok(()) };
        let request = super::super::config::config_request(&proto)?;
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(request.req_id.map_or(-1, i64::from)) { return r; }
        self.req_config_at(py, crate::client_core::session_level(), &request)
    }

    /// Update the configuration, as the official `updateConfigProtoBuf`
    /// (session level 221): an `UpdateConfigRequest` of the official
    /// library, or an object with its attributes; None does nothing. The
    /// answer is `update_config_response_proto_buf`. Below level 221, error
    /// 503 for the request, as the official client library answers, and
    /// nothing sent (ibx#612). From 221 a logon that forbids configuration
    /// access gets 321; otherwise the update is checked as the reference
    /// checks it, then a value ibx cannot follow is refused with the field
    /// named, and the rest is kept (decision of 18/10/2026 on ibx#610).
    #[pyo3(signature = (update_config_request_proto))]
    fn update_config_proto_buf(&self, py: Python<'_>, update_config_request_proto: Option<Bound<'_, PyAny>>) -> PyResult<()> {
        let Some(proto) = update_config_request_proto.filter(|p| !p.is_none()) else { return Ok(()) };
        let request = super::super::config::update_config_request(&proto)?;
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(request.req_id.map_or(-1, i64::from)) { return r; }
        self.update_config_at(py, crate::client_core::session_level(), &request)
    }

    // ── FA (Financial Advisor) ──

    fn request_fa(&self, py: Python<'_>, fa_data_type: i32) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if self.later(move |this, py| this.request_fa(py, fa_data_type)) { return Ok(()); }
        // Not an FA session: error 321 as the reference (ibx#481).
        let shared = self.shared_state()?;
        if !shared.reference.fa_session() {
            let (id, code, text) = crate::client_core::REQUEST_FA_NOT_FA;
            self.cb(py).call_method1(py, "error", (id, code, text, ""))?;
            return Ok(());
        }
        // From the session's FA data (ibx#507).
        match crate::client_core::fa_request(&shared, fa_data_type) {
            Ok(xml) => { self.cb(py).call_method1(py, "receive_fa", (fa_data_type, xml))?; }
            Err((id, code, text)) => { self.cb(py).call_method1(py, "error", (id, code, text, ""))?; }
        }
        Ok(())
    }

    #[pyo3(signature = (req_id, fa_data_type, cxml))]
    fn replace_fa(&self, py: Python<'_>, req_id: i64, fa_data_type: i32, cxml: &str) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(req_id) { return r; }
        let xml = cxml.to_string();
        if self.later(move |this, py| this.replace_fa(py, req_id, fa_data_type, &xml)) { return Ok(()); }
        if !crate::client_core::ClientCore::ids_fit("replace_fa", &[req_id]) { return Ok(()); }
        // Not an FA session: error 321 for the request as the reference
        // (ibx#481).
        let shared = self.shared_state()?;
        if !shared.reference.fa_session() {
            let (code, text) = crate::client_core::REPLACE_FA_NOT_FA;
            self.cb(py).call_method1(py, "error", (req_id, code, text, ""))?;
            return Ok(());
        }
        // Checked, replaced and saved; `replace_fa_end` when the saves are
        // answered (ibx#507).
        match crate::client_core::fa_replace(&shared, req_id, fa_data_type, cxml) {
            Ok(Some(save)) => {
                let tx = self.tx()?;
                super::send_cmd(py, &tx, save)?;
            }
            Ok(None) => {}
            Err((code, text)) => { self.cb(py).call_method1(py, "error", (req_id, code, text, ""))?; }
        }
        Ok(())
    }

    // ── Display Groups ──

    fn query_display_groups(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if self.later(move |this, py| this.query_display_groups(py, req_id)) { return Ok(()); }
        if !crate::client_core::ClientCore::ids_fit("query_display_groups", &[req_id]) { return Ok(()); }
        // The fixed list of groups, as the reference (ibx#424).
        match crate::client_core::ClientCore::query_display_groups(req_id) {
            Ok(groups) => { self.cb(py).call_method1(py, "display_group_list", (req_id, groups))?; }
            Err(text) => self.display_group_refused(py, &text)?,
        }
        Ok(())
    }

    /// As the reference (ibx#424): the contract of the group at once,
    /// `none` since no group has one; error 321 for a group outside 1 to 7
    /// or a request id already subscribed.
    fn subscribe_to_group_events(&self, py: Python<'_>, req_id: i64, group_id: i32) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if self.later(move |this, py| this.subscribe_to_group_events(py, req_id, group_id)) { return Ok(()); }
        if !crate::client_core::ClientCore::ids_fit("subscribe_to_group_events", &[req_id]) { return Ok(()); }
        match self.core.subscribe_to_group_events(req_id, group_id) {
            Ok(contract_info) => { self.cb(py).call_method1(py, "display_group_updated", (req_id, contract_info))?; }
            Err(text) => self.display_group_refused(py, &text)?,
        }
        Ok(())
    }

    /// No answer, but error 321 for a request id that is not subscribed,
    /// as the reference (ibx#424).
    fn unsubscribe_from_group_events(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("unsubscribe_from_group_events", &[req_id]) { return Ok(()); }
        if let Some(text) = self.core.unsubscribe_from_group_events(req_id) {
            self.display_group_refused(py, &text)?;
        }
        Ok(())
    }

    /// As the reference (ibx#424): error 321 for bad input or a request id
    /// that is not subscribed, error 473 for a conId that is not a
    /// contract, and no answer for a valid update, which changes no group.
    fn update_display_group(&self, py: Python<'_>, req_id: i64, contract_info: &str) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        use crate::client_core::DisplayGroupUpdate;
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("update_display_group", &[req_id]) { return Ok(()); }
        let shared = self.shared_state()?;
        let known = |con_id| shared.reference.get_contract(con_id).is_some();
        match self.core.update_display_group(req_id, contract_info, known) {
            DisplayGroupUpdate::Nothing => {}
            DisplayGroupUpdate::Refused(text) => self.display_group_refused(py, &text)?,
            DisplayGroupUpdate::Lookup(con_id) => {
                let tx = self.tx()?;
                super::send_cmd(py, &tx, crate::types::ControlCommand::DisplayGroupLookup { req_id, con_id })?;
            }
        }
        Ok(())
    }

    // ── Smart Components ──

    /// As the reference (ibx#441): the exchange map of the BBO exchange that
    /// market data made known; an unknown one gives error 321; a map not
    /// come yet is answered by the message loop, within 2 s.
    fn req_smart_components(&self, py: Python<'_>, req_id: i64, bbo_exchange: &str) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        let exchange = bbo_exchange.to_string();
        if self.later(move |this, py| this.req_smart_components(py, req_id, &exchange)) { return Ok(()); }
        if !crate::client_core::ClientCore::ids_fit("req_smart_components", &[req_id]) { return Ok(()); }
        let shared = self.shared_state()?;
        match self.core.req_smart_components(req_id, bbo_exchange, &shared) {
            Some(answer) => self.deliver_smart_components(py, req_id, answer),
            None => Ok(()),
        }
    }

    // ── News Providers ──

    fn req_news_providers(&self, py: Python<'_>) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if self.later(move |this, py| this.req_news_providers(py)) { return Ok(()); }
        let shared = self.shared_state()?;
        let np = shared.reference.news_providers();
        let mut providers: Vec<Py<NewsProviderPy>> = Vec::with_capacity(np.len());
        for p in np.iter() {
            let obj = NewsProviderPy { code: p.code.clone(), name: p.name.clone() };
            providers.push(Py::new(py, obj)?);
        }
        let py_list = pyo3::types::PyList::new(py, providers)?;
        self.cb(py).call_method1(py, "news_providers", (py_list.as_any(),))?;
        Ok(())
    }

    // ── Soft Dollar Tiers ──

    fn req_soft_dollar_tiers(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if self.later(move |this, py| this.req_soft_dollar_tiers(py, req_id)) { return Ok(()); }
        if !crate::client_core::ClientCore::ids_fit("req_soft_dollar_tiers", &[req_id]) { return Ok(()); }
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
        self.cb(py).call_method1(py, "soft_dollar_tiers", (req_id, py_list.as_any()))?;
        Ok(())
    }

    // ── Family Codes ──

    fn req_family_codes(&self, py: Python<'_>) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if self.later(move |this, py| this.req_family_codes(py)) { return Ok(()); }
        let shared = self.shared_state()?;
        let codes = shared.reference.family_codes();
        // Each with the official attributes, and still a pair (ibx#725).
        let entries = codes.iter()
            .map(|fc| super::super::named::family_code(py, &fc.account_id, &fc.family_code_str))
            .collect::<PyResult<Vec<_>>>()?;
        let py_list = pyo3::types::PyList::new(py, entries)?;
        self.cb(py).call_method1(py, "family_codes", (py_list.as_any(),))?;
        Ok(())
    }

    // ── Server Log Level ──

    #[pyo3(signature = (log_level=2))]
    fn set_server_log_level(&self, log_level: i32) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        let level = match log_level {
            1 => "error",
            2 => "warn",
            3 => "info",
            4 => "debug",
            5 => "trace",
            _ => "warn",
        };
        log::info!("set_server_log_level: {} (level {})", level, log_level);
        Ok(())
    }

    // ── User Info ──

    fn req_user_info(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if self.later(move |this, py| this.req_user_info(py, req_id)) { return Ok(()); }
        if !crate::client_core::ClientCore::ids_fit("req_user_info", &[req_id]) { return Ok(()); }
        let shared = self.shared_state()?;
        let id = shared.reference.white_branding_id();
        self.cb(py).call_method1(py, "user_info", (req_id, id))?;
        Ok(())
    }

    // ── WSH ──

    /// As the reference (ibx#443): error 10276 when the session has no WSH
    /// news source, 10277 when it is not subscribed; the meta data held is
    /// given at once; 10278 while a request is on its way; else the request
    /// goes to the server and its answer comes as `wsh_meta_data`.
    fn req_wsh_meta_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if self.later(move |this, py| this.req_wsh_meta_data(py, req_id)) { return Ok(()); }
        if !crate::client_core::ClientCore::ids_fit("req_wsh_meta_data", &[req_id]) { return Ok(()); }
        let step = self.core.wsh_meta_request(&*self.shared_state()?, req_id);
        self.wsh_step(py, req_id, true, step)
    }

    /// The request on its way is let go; no answer, as the reference
    /// (ibx#443).
    fn cancel_wsh_meta_data(&self, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        let _ = req_id;
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.wsh_cancel(&*self.shared_state()?);
        Ok(())
    }

    /// As the reference (ibx#443): the permission errors; 10282 before the
    /// meta data is held; 10309 unless exactly one of a contract and a
    /// filter is given; 10283 for a filter that is not a JSON object;
    /// nothing at all for a date that is not `yyyyMMdd`; 10281 while a
    /// request is on its way; else the events come as `wsh_event_data`.
    #[pyo3(signature = (req_id, wsh_event_data=None))]
    fn req_wsh_event_data(&self, py: Python<'_>, req_id: i64, wsh_event_data: Option<WshEventDataPy>) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        let data_later = wsh_event_data.clone();
        if self.later(move |this, py| this.req_wsh_event_data(py, req_id, data_later)) { return Ok(()); }
        if !crate::client_core::ClientCore::ids_fit("req_wsh_event_data", &[req_id]) { return Ok(()); }
        let d = wsh_event_data.unwrap_or_default();
        let request = crate::control::wsh::EventRequest {
            con_id: d.con_id, filter: d.filter, fill_watchlist: d.fill_watchlist,
            fill_portfolio: d.fill_portfolio, fill_competitors: d.fill_competitors,
            start_date: d.start_date, end_date: d.end_date, total_limit: d.total_limit,
        };
        let step = self.core.wsh_event_request(&*self.shared_state()?, req_id, &request);
        self.wsh_step(py, req_id, false, step)
    }

    /// As the reference (ibx#443): the meta data request on its way is
    /// let go, the event request goes on; no answer.
    fn cancel_wsh_event_data(&self, req_id: i64) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        let _ = req_id;
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.wsh_cancel(&*self.shared_state()?);
        Ok(())
    }
}

impl EClient {
    /// The outcome of a calendar data request (ibx#443).
    fn wsh_step(&self, py: Python<'_>, req_id: i64, meta: bool, step: crate::client_core::WshStep) -> PyResult<()> {
        use crate::client_core::WshStep;
        match step {
            WshStep::Refused(code, text) => { self.cb(py).call_method1(py, "error", (req_id, code, text.as_str(), ""))?; }
            WshStep::Answer(json) => { self.cb(py).call_method1(py, "wsh_meta_data", (req_id, json.as_str()))?; }
            WshStep::Send(body) => {
                let tx = self.tx()?;
                super::send_cmd(py, &tx, crate::types::ControlCommand::WshRequest { req_id, meta, body })?;
            }
            WshStep::Nothing => {}
        }
        Ok(())
    }

    /// `req_config_proto_buf` on a session of level `level` (ibx#610).
    pub(crate) fn req_config_at(&self, py: Python<'_>, level: i32, request: &crate::api::config::ConfigRequest) -> PyResult<()> {
        let shared = self.shared_state()?;
        match self.core.config_answer(level, request, &shared.reference.logon_features()) {
            Err((id, code, text)) if level < crate::client_core::LEVEL_CONFIG => {
                // The official client library's own check, on the caller's
                // thread before anything is sent: never put off (ibx#532).
                self.cb(py).call_method1(py, "error", (id, code, text, ""))?;
            }
            Err((id, code, text)) => shared.orders.push_order_error(id, code, text.to_string()),
            Ok(response) => self.give_config(py, response)?,
        }
        Ok(())
    }

    /// `update_config_proto_buf` on a session of level `level` (ibx#612).
    pub(crate) fn update_config_at(
        &self, py: Python<'_>, level: i32, request: &crate::api::config::UpdateConfigRequest,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        match self.core.update_config_answer(level, request, &shared.reference.logon_features()) {
            Err((id, code, text)) if level < crate::client_core::LEVEL_UPDATE_CONFIG => {
                // The official client library's own check, as above.
                self.cb(py).call_method1(py, "error", (id, code, text, ""))?;
            }
            Err((id, code, text)) => shared.orders.push_order_error(id, code, text.to_string()),
            Ok(response) => self.give_update_config(py, response)?,
        }
        Ok(())
    }

    /// The configuration to the wrapper, from the event loop (ibx#532).
    fn give_config(&self, py: Python<'_>, response: crate::api::config::ConfigResponse) -> PyResult<()> {
        let again = response.clone();
        if self.later(move |this, py| this.give_config(py, again)) { return Ok(()); }
        let object = super::super::config::config_response(py, &response)?;
        self.cb(py).call_method1(py, "config_response_proto_buf", (object,))?;
        Ok(())
    }

    /// The answer to an update to the wrapper, from the event loop.
    fn give_update_config(&self, py: Python<'_>, response: crate::api::config::UpdateConfigResponse) -> PyResult<()> {
        let again = response.clone();
        if self.later(move |this, py| this.give_update_config(py, again)) { return Ok(()); }
        let object = super::super::config::update_config_response(py, &response)?;
        self.cb(py).call_method1(py, "update_config_response_proto_buf", (object,))?;
        Ok(())
    }
}
