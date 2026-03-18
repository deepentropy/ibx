//! ibapi-compatible EWrapper base class with no-op default callbacks.

use pyo3::prelude::*;

/// ibapi-compatible EWrapper base class.
/// Users subclass this in Python and override callbacks they care about.
/// All methods are no-ops by default.
#[pyclass(subclass)]
pub struct EWrapper;

#[pymethods]
impl EWrapper {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn new(_args: &Bound<'_, pyo3::types::PyTuple>, _kwargs: Option<&Bound<'_, pyo3::types::PyDict>>) -> Self {
        Self
    }

    // ── Connection ──

    fn connect_ack(&self) {}

    fn connection_closed(&self) {}

    fn next_valid_id(&self, _order_id: i64) {}

    fn managed_accounts(&self, _accounts_list: &str) {}

    #[pyo3(signature = (_req_id, _error_code, _error_string, _advanced_order_reject_json=""))]
    fn error(&self, _req_id: i64, _error_code: i64, _error_string: &str, _advanced_order_reject_json: &str) {}

    fn current_time(&self, _time: i64) {}

    // ── Market Data ──

    fn tick_price(&self, _req_id: i64, _tick_type: i32, _price: f64, _attrib: PyObject) {}

    fn tick_size(&self, _req_id: i64, _tick_type: i32, _size: f64) {}

    fn tick_string(&self, _req_id: i64, _tick_type: i32, _value: &str) {}

    fn tick_generic(&self, _req_id: i64, _tick_type: i32, _value: f64) {}

    fn tick_snapshot_end(&self, _req_id: i64) {}

    fn market_data_type(&self, _req_id: i64, _market_data_type: i32) {}

    // ── Orders ──

    fn order_status(
        &self, _order_id: i64, _status: &str, _filled: f64, _remaining: f64,
        _avg_fill_price: f64, _perm_id: i64, _parent_id: i64,
        _last_fill_price: f64, _client_id: i64, _why_held: &str, _mkt_cap_price: f64,
    ) {}

    fn open_order(&self, _order_id: i64, _contract: PyObject, _order: PyObject, _order_state: PyObject) {}

    fn open_order_end(&self) {}

    fn exec_details(&self, _req_id: i64, _contract: PyObject, _execution: PyObject) {}

    fn exec_details_end(&self, _req_id: i64) {}

    fn commission_report(&self, _commission_report: PyObject) {}

    // ── Account ──

    fn update_account_value(&self, _key: &str, _value: &str, _currency: &str, _account_name: &str) {}

    fn update_portfolio(
        &self, _contract: PyObject, _position: f64, _market_price: f64,
        _market_value: f64, _average_cost: f64, _unrealized_pnl: f64,
        _realized_pnl: f64, _account_name: &str,
    ) {}

    fn update_account_time(&self, _timestamp: &str) {}

    fn account_download_end(&self, _account: &str) {}

    fn account_summary(&self, _req_id: i64, _account: &str, _tag: &str, _value: &str, _currency: &str) {}

    fn account_summary_end(&self, _req_id: i64) {}

    fn position(&self, _account: &str, _contract: PyObject, _pos: f64, _avg_cost: f64) {}

    fn position_end(&self) {}

    fn pnl(&self, _req_id: i64, _daily_pnl: f64, _unrealized_pnl: f64, _realized_pnl: f64) {}

    fn pnl_single(
        &self, _req_id: i64, _pos: f64, _daily_pnl: f64,
        _unrealized_pnl: f64, _realized_pnl: f64, _value: f64,
    ) {}

    // ── Historical Data ──

    fn historical_data(&self, _req_id: i64, _bar: PyObject) {}

    fn historical_data_end(&self, _req_id: i64, _start: &str, _end: &str) {}

    fn historical_data_update(&self, _req_id: i64, _bar: PyObject) {}

    fn head_timestamp(&self, _req_id: i64, _head_timestamp: &str) {}

    // ── Contract Details ──

    fn contract_details(&self, _req_id: i64, _contract_details: PyObject) {}

    fn contract_details_end(&self, _req_id: i64) {}

    fn symbol_samples(&self, _req_id: i64, _contract_descriptions: PyObject) {}

    // ── Tick-by-Tick ──

    fn tick_by_tick_all_last(
        &self, _req_id: i64, _tick_type: i32, _time: i64, _price: f64,
        _size: f64, _tick_attrib_last: PyObject, _exchange: &str, _special_conditions: &str,
    ) {}

    fn tick_by_tick_bid_ask(
        &self, _req_id: i64, _time: i64, _bid_price: f64, _ask_price: f64,
        _bid_size: f64, _ask_size: f64, _tick_attrib_bid_ask: PyObject,
    ) {}

    fn tick_by_tick_mid_point(&self, _req_id: i64, _time: i64, _mid_point: f64) {}

    // ── Scanner ──

    fn scanner_data(
        &self, _req_id: i64, _rank: i32, _contract_details: PyObject,
        _distance: &str, _benchmark: &str, _projection: &str, _legs_str: &str,
    ) {}

    fn scanner_data_end(&self, _req_id: i64) {}

    fn scanner_parameters(&self, _xml: &str) {}

    // ── News ──

    fn news_providers(&self, _news_providers: PyObject) {}

    fn news_article(&self, _req_id: i64, _article_type: i32, _article_text: &str) {}

    fn historical_news(
        &self, _req_id: i64, _time: &str, _provider_code: &str,
        _article_id: &str, _headline: &str,
    ) {}

    fn historical_news_end(&self, _req_id: i64, _has_more: bool) {}

    fn tick_news(
        &self, _ticker_id: i64, _time_stamp: i64, _provider_code: &str,
        _article_id: &str, _headline: &str, _extra_data: &str,
    ) {}

    // ── Market Depth ──

    fn update_mkt_depth(
        &self, _req_id: i64, _position: i32, _operation: i32,
        _side: i32, _price: f64, _size: f64,
    ) {}

    fn update_mkt_depth_l2(
        &self, _req_id: i64, _position: i32, _market_maker: &str,
        _operation: i32, _side: i32, _price: f64, _size: f64, _is_smart_depth: bool,
    ) {}

    // ── Market Depth (additional) ──

    fn mkt_depth_exchanges(&self, _depth_mkt_data_descriptions: PyObject) {}

    // ── Real-Time Bars ──

    fn real_time_bar(
        &self, _req_id: i64, _date: i64, _open: f64, _high: f64,
        _low: f64, _close: f64, _volume: f64, _wap: f64, _count: i32,
    ) {}

    // ── Historical Ticks ──

    fn historical_ticks(&self, _req_id: i64, _ticks: PyObject, _done: bool) {}

    fn historical_ticks_bid_ask(&self, _req_id: i64, _ticks: PyObject, _done: bool) {}

    fn historical_ticks_last(&self, _req_id: i64, _ticks: PyObject, _done: bool) {}

    // ── Options ──

    fn tick_option_computation(
        &self, _req_id: i64, _tick_type: i32, _tick_attrib: i32,
        _implied_vol: f64, _delta: f64, _opt_price: f64, _pv_dividend: f64,
        _gamma: f64, _vega: f64, _theta: f64, _und_price: f64,
    ) {}

    fn security_definition_option_parameter(
        &self, _req_id: i64, _exchange: &str, _underlying_con_id: i64,
        _trading_class: &str, _multiplier: &str, _expirations: PyObject, _strikes: PyObject,
    ) {}

    fn security_definition_option_parameter_end(&self, _req_id: i64) {}

    // ── Fundamental Data ──

    fn fundamental_data(&self, _req_id: i64, _data: &str) {}

    // ── News Bulletins ──

    fn update_news_bulletin(&self, _msg_id: i64, _msg_type: i32, _message: &str, _orig_exchange: &str) {}

    // ── Financial Advisor ──

    fn receive_fa(&self, _fa_data_type: i32, _xml: &str) {}

    fn replace_fa_end(&self, _req_id: i64, _text: &str) {}

    // ── Multi-Account / Multi-Model ──

    fn position_multi(&self, _req_id: i64, _account: &str, _model_code: &str, _contract: PyObject, _pos: f64, _avg_cost: f64) {}

    fn position_multi_end(&self, _req_id: i64) {}

    fn account_update_multi(&self, _req_id: i64, _account: &str, _model_code: &str, _key: &str, _value: &str, _currency: &str) {}

    fn account_update_multi_end(&self, _req_id: i64) {}

    // ── Tier 3: Display Groups ──

    fn display_group_list(&self, _req_id: i64, _groups: &str) {}

    fn display_group_updated(&self, _req_id: i64, _contract_info: &str) {}

    // ── Tier 3: Market Rules ──

    fn market_rule(&self, _market_rule_id: i64, _price_increments: PyObject) {}

    // ── Tier 3: Smart Components ──

    fn smart_components(&self, _req_id: i64, _smart_component_map: PyObject) {}

    // ── Tier 3: Soft Dollar Tiers ──

    fn soft_dollar_tiers(&self, _req_id: i64, _tiers: PyObject) {}

    // ── Tier 3: Family Codes ──

    fn family_codes(&self, _family_codes: PyObject) {}

    // ── Tier 3: Histogram Data ──

    fn histogram_data(&self, _req_id: i64, _items: PyObject) {}

    // ── Tier 3: User Info ──

    fn user_info(&self, _req_id: i64, _white_branding_id: &str) {}

    // ── Tier 3: WSH ──

    fn wsh_meta_data(&self, _req_id: i64, _data_json: &str) {}

    fn wsh_event_data(&self, _req_id: i64, _data_json: &str) {}

    // ── Tier 3: Completed Orders ──

    fn completed_order(&self, _contract: PyObject, _order: PyObject, _order_state: PyObject) {}

    fn completed_orders_end(&self) {}

    // ── Tier 3: Order Bound ──

    fn order_bound(&self, _order_id: i64, _api_client_id: i64, _api_order_id: i64) {}

    // ── Tier 3: Tick Req Params ──

    fn tick_req_params(&self, _ticker_id: i64, _min_tick: f64, _bbo_exchange: &str, _snapshot_permissions: i64) {}

    // ── Tier 3: Bond Contract Details ──

    fn bond_contract_details(&self, _req_id: i64, _contract_details: PyObject) {}

    // ── Tier 3: Delta Neutral Validation ──

    fn delta_neutral_validation(&self, _req_id: i64, _delta_neutral_contract: PyObject) {}

    // ── Tier 3: Historical Schedule ──

    fn historical_schedule(&self, _req_id: i64, _start_date_time: &str, _end_date_time: &str, _time_zone: &str, _sessions: PyObject) {}
}

/// Register EWrapper on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<EWrapper>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ewrapper_can_be_constructed() {
        let _w = EWrapper::new();
    }
}
