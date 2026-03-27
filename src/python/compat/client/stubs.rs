//! Gateway-local fakes and pure no-op stubs.

use pyo3::prelude::*;

use super::EClient;
use super::super::contract::Contract;

#[pymethods]
impl EClient {
    // ── Options Calculations (stubs) ──

    #[pyo3(signature = (req_id, contract, option_price, under_price, implied_vol_options=Vec::new()))]
    fn calculate_implied_volatility(
        &self, req_id: i64, contract: &Contract, option_price: f64,
        under_price: f64, implied_vol_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let _ = (req_id, contract, option_price, under_price, implied_vol_options);
        log::warn!("calculate_implied_volatility: not yet implemented in engine");
        Ok(())
    }

    #[pyo3(signature = (req_id, contract, volatility, under_price, opt_prc_options=Vec::new()))]
    fn calculate_option_price(
        &self, req_id: i64, contract: &Contract, volatility: f64,
        under_price: f64, opt_prc_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let _ = (req_id, contract, volatility, under_price, opt_prc_options);
        log::warn!("calculate_option_price: not yet implemented in engine");
        Ok(())
    }

    fn cancel_calculate_implied_volatility(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        Ok(())
    }

    fn cancel_calculate_option_price(&self, req_id: i64) -> PyResult<()> {
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

    // ── Option Chain Parameters (stub) ──

    #[pyo3(signature = (req_id, underlying_symbol, fut_fop_exchange="", underlying_sec_type="STK", underlying_con_id=0))]
    fn req_sec_def_opt_params(
        &self,
        req_id: i64,
        underlying_symbol: &str,
        fut_fop_exchange: &str,
        underlying_sec_type: &str,
        underlying_con_id: i64,
    ) -> PyResult<()> {
        let _ = (req_id, underlying_symbol, fut_fop_exchange, underlying_sec_type, underlying_con_id);
        log::warn!("req_sec_def_opt_params: not yet implemented in engine");
        Ok(())
    }

    // ── News Bulletins ──

    #[pyo3(signature = (all_msgs=true))]
    fn req_news_bulletins(&self, all_msgs: bool) -> PyResult<()> {
        let _ = all_msgs;
        self.core.subscribe_bulletins();
        Ok(())
    }

    fn cancel_news_bulletins(&self) -> PyResult<()> {
        self.core.unsubscribe_bulletins();
        Ok(())
    }

    // ── Server Time ──

    fn req_current_time(&self, py: Python<'_>) -> PyResult<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.wrapper.call_method1(py, "current_time", (now,))?;
        Ok(())
    }

    // ── FA (Financial Advisor) ──

    fn request_fa(&self, _fa_data_type: i32) -> PyResult<()> {
        log::warn!("request_fa: not yet implemented — needs FIX capture");
        Ok(())
    }

    #[pyo3(signature = (req_id, fa_data_type, cxml))]
    fn replace_fa(&self, req_id: i64, fa_data_type: i32, cxml: &str) -> PyResult<()> {
        let _ = (req_id, fa_data_type, cxml);
        log::warn!("replace_fa: not yet implemented — needs FIX capture");
        Ok(())
    }

    // ── Display Groups ──

    fn query_display_groups(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        self.wrapper.call_method1(py, "display_group_list", (req_id, ""))?;
        Ok(())
    }

    fn subscribe_to_group_events(&self, req_id: i64, group_id: i32) -> PyResult<()> {
        let _ = (req_id, group_id);
        Ok(())
    }

    fn unsubscribe_from_group_events(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        Ok(())
    }

    fn update_display_group(&self, req_id: i64, contract_info: &str) -> PyResult<()> {
        let _ = (req_id, contract_info);
        Ok(())
    }

    // ── Smart Components ──

    fn req_smart_components(&self, py: Python<'_>, req_id: i64, bbo_exchange: &str) -> PyResult<()> {
        let _ = bbo_exchange;
        let shared = self.shared_state()?;
        let sc = shared.reference.smart_components();
        let components = pyo3::types::PyList::new(py, sc.iter().map(|c| {
            let dict = pyo3::types::PyDict::new(py);
            dict.set_item("bitNumber", c.bit_number).unwrap();
            dict.set_item("exchange", &c.exchange).unwrap();
            dict.set_item("exchangeLetter", &c.exchange_letter).unwrap();
            dict
        }))?;
        self.wrapper.call_method1(py, "smart_components", (req_id, components.as_any()))?;
        Ok(())
    }

    // ── News Providers ──

    fn req_news_providers(&self, py: Python<'_>) -> PyResult<()> {
        let shared = self.shared_state()?;
        let np = shared.reference.news_providers();
        let py_list = pyo3::types::PyList::new(py, np.iter().map(|p| {
            let dict = pyo3::types::PyDict::new(py);
            dict.set_item("code", &p.code).unwrap();
            dict.set_item("name", &p.name).unwrap();
            dict
        }))?;
        self.wrapper.call_method1(py, "news_providers", (py_list.as_any(),))?;
        Ok(())
    }

    // ── Soft Dollar Tiers ──

    fn req_soft_dollar_tiers(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let shared = self.shared_state()?;
        let tiers = shared.reference.soft_dollar_tiers();
        let py_list = pyo3::types::PyList::new(py, tiers.iter().map(|t| {
            let dict = pyo3::types::PyDict::new(py);
            dict.set_item("name", &t.name).unwrap();
            dict.set_item("val", &t.val).unwrap();
            dict.set_item("displayName", &t.display_name).unwrap();
            dict
        }))?;
        self.wrapper.call_method1(py, "soft_dollar_tiers", (req_id, py_list.as_any()))?;
        Ok(())
    }

    // ── Family Codes ──

    fn req_family_codes(&self, py: Python<'_>) -> PyResult<()> {
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
        let shared = self.shared_state()?;
        let id = shared.reference.white_branding_id();
        self.wrapper.call_method1(py, "user_info", (req_id, id))?;
        Ok(())
    }

    // ── WSH ──

    fn req_wsh_meta_data(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        log::warn!("req_wsh_meta_data: not yet implemented — needs FIX capture");
        Ok(())
    }

    #[pyo3(signature = (req_id, wsh_event_data=None))]
    fn req_wsh_event_data(&self, req_id: i64, wsh_event_data: Option<PyObject>) -> PyResult<()> {
        let _ = (req_id, wsh_event_data);
        log::warn!("req_wsh_event_data: not yet implemented — needs FIX capture");
        Ok(())
    }
}
