//! Gateway-local fakes and pure no-op stubs.

use pyo3::prelude::*;

use super::EClient;
use super::super::contract::{Contract, NewsProviderPy, SmartComponentPy, SoftDollarTierPy, WshEventDataPy};

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
                self.wrapper.call_method1(py, "error", (req_id, code, msg.as_str(), ""))?;
                return Ok(());
            }
        };
        let map = pyo3::types::PyDict::new(py);
        for c in sc.iter() {
            let obj = SmartComponentPy {
                bit_number: c.bit_number,
                exchange: c.exchange.clone(),
                exchange_letter: c.exchange_letter.clone(),
            };
            map.set_item(c.bit_number, Py::new(py, obj)?)?;
        }
        self.wrapper.call_method1(py, "smart_components", (req_id, map.as_any()))?;
        Ok(())
    }

    /// The error of a refused display group request (ibx#424).
    fn display_group_refused(&self, py: Python<'_>, text: &str) -> PyResult<()> {
        let (id, code) = crate::client_core::DISPLAY_GROUP_REFUSAL;
        self.wrapper.call_method1(py, "error", (id, code, text, ""))?;
        Ok(())
    }

    fn calculate_option(
        &self, py: Python<'_>, req_id: i64, contract: &Contract,
        kind: crate::control::optcalc::CalcKind, under_price: f64,
    ) -> PyResult<()> {
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
        let request = crate::types::ControlCommand::CalcOption { req_id, con_id: contract.con_id, kind, under_price };
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
        let _ = opt_prc_options;
        self.calculate_option(py, req_id, contract, crate::control::optcalc::CalcKind::Price { volatility }, under_price)
    }

    fn cancel_calculate_implied_volatility(&self, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(req_id) { return r; }
        let _ = req_id;
        Ok(())
    }

    fn cancel_calculate_option_price(&self, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(req_id) { return r; }
        let _ = req_id;
        Ok(())
    }

    #[pyo3(signature = (req_id, contract, exercise_action, exercise_quantity, account, _override))]
    fn exercise_options(
        &self, req_id: i64, contract: &Contract, exercise_action: i32,
        exercise_quantity: i32, account: &str, _override: i32,
    ) -> PyResult<()> {
        let _ = (req_id, contract, exercise_action, exercise_quantity, account, _override);
        log::warn!("exercise_options: not yet implemented in engine");
        Ok(())
    }


    // ── News Bulletins ──

    #[pyo3(signature = (all_msgs=true))]
    fn req_news_bulletins(&self, all_msgs: bool) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.subscribe_bulletins(all_msgs);
        Ok(())
    }

    fn cancel_news_bulletins(&self) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.unsubscribe_bulletins();
        Ok(())
    }

    // ── Server Time ──

    fn req_current_time(&self, py: Python<'_>) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        // The local clock plus the offset to the server clock of the
        // logon, as the reference (ibx#421).
        let now = self.shared_state()?.reference.server_time_secs();
        self.wrapper.call_method1(py, "current_time", (now,))?;
        Ok(())
    }

    // ── FA (Financial Advisor) ──

    fn request_fa(&self, py: Python<'_>, _fa_data_type: i32) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        // Not an FA session: error 321 as the reference (ibx#481).
        if !self.shared_state()?.reference.fa_session() {
            let (id, code, text) = crate::client_core::REQUEST_FA_NOT_FA;
            self.wrapper.call_method1(py, "error", (id, code, text, ""))?;
            return Ok(());
        }
        log::warn!("request_fa: not yet implemented — needs FIX capture");
        Ok(())
    }

    #[pyo3(signature = (req_id, fa_data_type, cxml))]
    fn replace_fa(&self, py: Python<'_>, req_id: i64, fa_data_type: i32, cxml: &str) -> PyResult<()> {
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("replace_fa", &[req_id]) { return Ok(()); }
        // Not an FA session: error 321 for the request as the reference
        // (ibx#481).
        if !self.shared_state()?.reference.fa_session() {
            let (code, text) = crate::client_core::REPLACE_FA_NOT_FA;
            self.wrapper.call_method1(py, "error", (req_id, code, text, ""))?;
            return Ok(());
        }
        let _ = (fa_data_type, cxml);
        log::warn!("replace_fa: not yet implemented — needs FIX capture");
        Ok(())
    }

    // ── Display Groups ──

    fn query_display_groups(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("query_display_groups", &[req_id]) { return Ok(()); }
        // The fixed list of groups, as the reference (ibx#424).
        match crate::client_core::ClientCore::query_display_groups(req_id) {
            Ok(groups) => { self.wrapper.call_method1(py, "display_group_list", (req_id, groups))?; }
            Err(text) => self.display_group_refused(py, &text)?,
        }
        Ok(())
    }

    /// As the reference (ibx#424): the contract of the group at once,
    /// `none` since no group has one; error 321 for a group outside 1 to 7
    /// or a request id already subscribed.
    fn subscribe_to_group_events(&self, py: Python<'_>, req_id: i64, group_id: i32) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("subscribe_to_group_events", &[req_id]) { return Ok(()); }
        match self.core.subscribe_to_group_events(req_id, group_id) {
            Ok(contract_info) => { self.wrapper.call_method1(py, "display_group_updated", (req_id, contract_info))?; }
            Err(text) => self.display_group_refused(py, &text)?,
        }
        Ok(())
    }

    /// No answer, but error 321 for a request id that is not subscribed,
    /// as the reference (ibx#424).
    fn unsubscribe_from_group_events(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
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
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_smart_components", &[req_id]) { return Ok(()); }
        let shared = self.shared_state()?;
        match self.core.req_smart_components(req_id, bbo_exchange, &shared) {
            Some(answer) => self.deliver_smart_components(py, req_id, answer),
            None => Ok(()),
        }
    }

    // ── News Providers ──

    fn req_news_providers(&self, py: Python<'_>) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let shared = self.shared_state()?;
        let np = shared.reference.news_providers();
        let mut providers: Vec<Py<NewsProviderPy>> = Vec::with_capacity(np.len());
        for p in np.iter() {
            let obj = NewsProviderPy { code: p.code.clone(), name: p.name.clone() };
            providers.push(Py::new(py, obj)?);
        }
        let py_list = pyo3::types::PyList::new(py, providers)?;
        self.wrapper.call_method1(py, "news_providers", (py_list.as_any(),))?;
        Ok(())
    }

    // ── Soft Dollar Tiers ──

    fn req_soft_dollar_tiers(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
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
        self.wrapper.call_method1(py, "soft_dollar_tiers", (req_id, py_list.as_any()))?;
        Ok(())
    }

    // ── Family Codes ──

    fn req_family_codes(&self, py: Python<'_>) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let shared = self.shared_state()?;
        let codes = shared.reference.family_codes();
        let py_list = pyo3::types::PyList::new(py, codes.iter().map(|fc| {
            pyo3::types::PyTuple::new(py, &[
                fc.account_id.as_str().into_pyobject(py).unwrap().into_any(),
                fc.family_code_str.as_str().into_pyobject(py).unwrap().into_any(),
            ]).unwrap()
        }))?;
        self.wrapper.call_method1(py, "family_codes", (py_list.as_any(),))?;
        Ok(())
    }

    // ── Server Log Level ──

    #[pyo3(signature = (log_level=2))]
    fn set_server_log_level(&self, log_level: i32) -> PyResult<()> {
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
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_user_info", &[req_id]) { return Ok(()); }
        let shared = self.shared_state()?;
        let id = shared.reference.white_branding_id();
        self.wrapper.call_method1(py, "user_info", (req_id, id))?;
        Ok(())
    }

    // ── WSH ──

    /// The permission check of the reference (ibx#443): error 10276 when
    /// the session has no WSH news source, 10277 when it is not
    /// subscribed. The data request itself is not implemented: with the
    /// permission, error 10279.
    fn req_wsh_meta_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_wsh_meta_data", &[req_id]) { return Ok(()); }
        let (code, text) = crate::client_core::wsh_meta_data_error(&self.shared_state()?.reference);
        self.wrapper.call_method1(py, "error", (req_id, code, text, ""))?;
        Ok(())
    }

    /// No answer, as the reference (ibx#443).
    fn cancel_wsh_meta_data(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        if let Some(r) = self.not_connected(-1) { return r; }
        Ok(())
    }

    /// The permission check of the reference (ibx#443), as
    /// `req_wsh_meta_data`. The data request itself is not implemented:
    /// with the permission, error 10282, since no meta data is held.
    #[pyo3(signature = (req_id, wsh_event_data=None))]
    fn req_wsh_event_data(&self, py: Python<'_>, req_id: i64, wsh_event_data: Option<WshEventDataPy>) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_wsh_event_data", &[req_id]) { return Ok(()); }
        let _ = wsh_event_data;
        let (code, text) = crate::client_core::wsh_event_data_error(&self.shared_state()?.reference);
        self.wrapper.call_method1(py, "error", (req_id, code, text, ""))?;
        Ok(())
    }

    /// No answer, as the reference (ibx#443).
    fn cancel_wsh_event_data(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        if let Some(r) = self.not_connected(-1) { return r; }
        Ok(())
    }
}
