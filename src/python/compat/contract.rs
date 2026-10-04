//! ibapi-compatible Contract, Order, TagValue, and condition classes.

use pyo3::prelude::*;
use pyo3::exceptions::PyRuntimeError;

use crate::types::*;
use super::super::types::PRICE_SCALE_F;
use super::values::{
    attr, decimal_from_py, decimal_text_from_py, decimal_text_to_py, decimal_to_py, empty_list, items,
    list_or_none, official_names,
};

// ── Contract ──

/// ibapi-compatible Contract class.
#[pyclass(from_py_object)]
pub struct Contract {
    #[pyo3(get, set)]
    pub con_id: i64,
    #[pyo3(get, set)]
    pub symbol: String,
    #[pyo3(get, set)]
    pub sec_type: String,
    #[pyo3(get, set)]
    pub exchange: String,
    #[pyo3(get, set)]
    pub currency: String,
    #[pyo3(get, set)]
    pub last_trade_date_or_contract_month: String,
    #[pyo3(get, set)]
    pub last_trade_date: String,
    #[pyo3(get, set)]
    pub strike: f64,
    #[pyo3(get, set)]
    pub right: String,
    #[pyo3(get, set)]
    pub multiplier: String,
    #[pyo3(get, set)]
    pub local_symbol: String,
    #[pyo3(get, set)]
    pub primary_exchange: String,
    #[pyo3(get, set)]
    pub trading_class: String,
    #[pyo3(get, set)]
    pub include_expired: bool,
    #[pyo3(get, set)]
    pub sec_id_type: String,
    #[pyo3(get, set)]
    pub sec_id: String,
    #[pyo3(get, set)]
    pub description: String,
    #[pyo3(get, set)]
    pub issuer_id: String,
    #[pyo3(get, set)]
    pub combo_legs_descrip: String,
    /// The list itself, as the official API's: `comboLegs.append(leg)`
    /// changes the contract.
    #[pyo3(get, set)]
    pub combo_legs: Py<PyAny>,
    #[pyo3(get, set)]
    pub delta_neutral_contract: Py<PyAny>,
}

impl Clone for Contract {
    fn clone(&self) -> Self {
        Self {
            con_id: self.con_id,
            symbol: self.symbol.clone(),
            sec_type: self.sec_type.clone(),
            exchange: self.exchange.clone(),
            currency: self.currency.clone(),
            last_trade_date_or_contract_month: self.last_trade_date_or_contract_month.clone(),
            last_trade_date: self.last_trade_date.clone(),
            strike: self.strike,
            right: self.right.clone(),
            multiplier: self.multiplier.clone(),
            local_symbol: self.local_symbol.clone(),
            primary_exchange: self.primary_exchange.clone(),
            trading_class: self.trading_class.clone(),
            include_expired: self.include_expired,
            sec_id_type: self.sec_id_type.clone(),
            sec_id: self.sec_id.clone(),
            description: self.description.clone(),
            issuer_id: self.issuer_id.clone(),
            combo_legs_descrip: self.combo_legs_descrip.clone(),
            // The legs of a combo are kept (ibx#470).
            combo_legs: Python::attach(|py| self.combo_legs.clone_ref(py)),
            delta_neutral_contract: Python::attach(|py| self.delta_neutral_contract.clone_ref(py)),
        }
    }
}

/// The official API's defaults (ibapi 10.46 `Contract()`): empty texts,
/// the strike unset (`f64::MAX`), no legs.
impl Default for Contract {
    fn default() -> Self {
        Self {
            con_id: 0,
            symbol: String::new(),
            sec_type: String::new(),
            exchange: String::new(),
            currency: String::new(),
            last_trade_date_or_contract_month: String::new(),
            last_trade_date: String::new(),
            strike: f64::MAX,
            right: String::new(),
            multiplier: String::new(),
            local_symbol: String::new(),
            primary_exchange: String::new(),
            trading_class: String::new(),
            include_expired: false,
            sec_id_type: String::new(),
            sec_id: String::new(),
            description: String::new(),
            issuer_id: String::new(),
            combo_legs_descrip: String::new(),
            combo_legs: empty_list(),
            delta_neutral_contract: Python::attach(|py| py.None()),
        }
    }
}

#[pymethods]
impl Contract {
    #[new]
    #[pyo3(signature = (con_id=0, symbol="".to_string(), sec_type="".to_string(), exchange="".to_string(), currency="".to_string(), last_trade_date_or_contract_month="".to_string(), strike=f64::MAX, right="".to_string(), multiplier="".to_string(), local_symbol="".to_string(), primary_exchange="".to_string(), trading_class="".to_string()))]
    fn new(
        con_id: i64,
        symbol: String,
        sec_type: String,
        exchange: String,
        currency: String,
        last_trade_date_or_contract_month: String,
        strike: f64,
        right: String,
        multiplier: String,
        local_symbol: String,
        primary_exchange: String,
        trading_class: String,
    ) -> Self {
        Self {
            con_id,
            symbol,
            sec_type,
            exchange,
            currency,
            last_trade_date_or_contract_month,
            strike,
            right,
            multiplier,
            local_symbol,
            primary_exchange,
            trading_class,
            ..Default::default()
        }
    }

    fn __repr__(&self) -> String {
        format!("Contract(conId={}, symbol='{}', secType='{}', exchange='{}')",
            self.con_id, self.symbol, self.sec_type, self.exchange)
    }

    // ibapi camelCase aliases
    #[getter(conId)]
    fn get_con_id_alias(&self) -> i64 { self.con_id }
    #[setter(conId)]
    fn set_con_id_alias(&mut self, v: i64) { self.con_id = v; }
    #[getter(secType)]
    fn get_sec_type_alias(&self) -> String { self.sec_type.clone() }
    #[setter(secType)]
    fn set_sec_type_alias(&mut self, v: String) { self.sec_type = v; }
    #[getter(lastTradeDateOrContractMonth)]
    fn get_ltdocm_alias(&self) -> String { self.last_trade_date_or_contract_month.clone() }
    #[setter(lastTradeDateOrContractMonth)]
    fn set_ltdocm_alias(&mut self, v: String) { self.last_trade_date_or_contract_month = v; }
    #[getter(lastTradeDate)]
    fn get_ltd_alias(&self) -> String { self.last_trade_date.clone() }
    #[setter(lastTradeDate)]
    fn set_ltd_alias(&mut self, v: String) { self.last_trade_date = v; }
    #[getter(localSymbol)]
    fn get_local_symbol_alias(&self) -> String { self.local_symbol.clone() }
    #[setter(localSymbol)]
    fn set_local_symbol_alias(&mut self, v: String) { self.local_symbol = v; }
    #[getter(primaryExchange)]
    fn get_primary_exchange_alias(&self) -> String { self.primary_exchange.clone() }
    #[setter(primaryExchange)]
    fn set_primary_exchange_alias(&mut self, v: String) { self.primary_exchange = v; }
    #[getter(tradingClass)]
    fn get_trading_class_alias(&self) -> String { self.trading_class.clone() }
    #[setter(tradingClass)]
    fn set_trading_class_alias(&mut self, v: String) { self.trading_class = v; }
    #[getter(includeExpired)]
    fn get_include_expired_alias(&self) -> bool { self.include_expired }
    #[setter(includeExpired)]
    fn set_include_expired_alias(&mut self, v: bool) { self.include_expired = v; }
    #[getter(secIdType)]
    fn get_sec_id_type_alias(&self) -> String { self.sec_id_type.clone() }
    #[setter(secIdType)]
    fn set_sec_id_type_alias(&mut self, v: String) { self.sec_id_type = v; }
    #[getter(secId)]
    fn get_sec_id_alias(&self) -> String { self.sec_id.clone() }
    #[setter(secId)]
    fn set_sec_id_alias(&mut self, v: String) { self.sec_id = v; }
    #[getter(issuerId)]
    fn get_issuer_id_alias(&self) -> String { self.issuer_id.clone() }
    #[setter(issuerId)]
    fn set_issuer_id_alias(&mut self, v: String) { self.issuer_id = v; }
    #[getter(comboLegsDescrip)]
    fn get_combo_legs_descrip_alias(&self) -> String { self.combo_legs_descrip.clone() }
    #[setter(comboLegsDescrip)]
    fn set_combo_legs_descrip_alias(&mut self, v: String) { self.combo_legs_descrip = v; }
    #[getter(comboLegs)]
    fn get_combo_legs_alias(&self, py: Python<'_>) -> Py<PyAny> { self.combo_legs.clone_ref(py) }
    #[setter(comboLegs)]
    fn set_combo_legs_alias(&mut self, v: Py<PyAny>) { self.combo_legs = v; }
    #[getter(deltaNeutralContract)]
    fn get_delta_neutral_alias(&self, py: Python<'_>) -> Py<PyAny> { self.delta_neutral_contract.clone_ref(py) }
    #[setter(deltaNeutralContract)]
    fn set_delta_neutral_alias(&mut self, v: Py<PyAny>) { self.delta_neutral_contract = v; }
}

// ── Order ──

/// ibapi-compatible Order class.
#[pyclass(from_py_object)]
pub struct Order {
    // ── Original fields ──
    #[pyo3(get, set)]
    pub order_id: i64,
    #[pyo3(get, set)]
    pub action: String,
    /// The official API's Decimal: `decimal.Decimal` in Python, unset
    /// `f64::MAX` here.
    pub total_quantity: f64,
    #[pyo3(get, set)]
    pub order_type: String,
    #[pyo3(get, set)]
    pub lmt_price: f64,
    #[pyo3(get, set)]
    pub aux_price: f64,
    #[pyo3(get, set)]
    pub tif: String,
    #[pyo3(get, set)]
    pub outside_rth: bool,
    #[pyo3(get, set)]
    pub display_size: i32,
    #[pyo3(get, set)]
    pub min_qty: i32,
    #[pyo3(get, set)]
    pub hidden: bool,
    #[pyo3(get, set)]
    pub good_after_time: String,
    #[pyo3(get, set)]
    pub good_till_date: String,
    #[pyo3(get, set)]
    pub oca_group: String,
    #[pyo3(get, set)]
    pub trailing_percent: f64,
    #[pyo3(get, set)]
    pub algo_strategy: String,
    /// The list itself (None by default), as the official API's.
    #[pyo3(get, set)]
    pub algo_params: Py<PyAny>,
    #[pyo3(get, set)]
    pub what_if: bool,
    #[pyo3(get, set)]
    pub cash_qty: f64,
    #[pyo3(get, set)]
    pub parent_id: i64,
    #[pyo3(get, set)]
    pub transmit: bool,
    #[pyo3(get, set)]
    pub discretionary_amt: f64,
    #[pyo3(get, set)]
    pub sweep_to_fill: bool,
    #[pyo3(get, set)]
    pub all_or_none: bool,
    #[pyo3(get, set)]
    pub trigger_method: i32,
    #[pyo3(get, set)]
    pub adjusted_order_type: String,
    #[pyo3(get, set)]
    pub trigger_price: f64,
    #[pyo3(get, set)]
    pub adjusted_stop_price: f64,
    #[pyo3(get, set)]
    pub adjusted_stop_limit_price: f64,
    /// The list itself, as the official API's: `conditions.append(c)`
    /// changes the order.
    #[pyo3(get, set)]
    pub conditions: Py<PyAny>,
    #[pyo3(get, set)]
    pub conditions_ignore_rth: bool,
    #[pyo3(get, set)]
    pub conditions_cancel_order: bool,

    // ── New fields (ibapi ground truth) ──
    #[pyo3(get, set)]
    pub account: String,
    #[pyo3(get, set)]
    pub active_start_time: String,
    #[pyo3(get, set)]
    pub active_stop_time: String,
    #[pyo3(get, set)]
    pub adjustable_trailing_unit: i32,
    #[pyo3(get, set)]
    pub adjusted_trailing_amount: f64,
    #[pyo3(get, set)]
    pub advanced_error_override: String,
    #[pyo3(get, set)]
    pub algo_id: String,
    #[pyo3(get, set)]
    pub allow_pre_open: bool,
    #[pyo3(get, set)]
    pub auction_strategy: i32,
    #[pyo3(get, set)]
    pub auto_cancel_date: String,
    #[pyo3(get, set)]
    pub auto_cancel_parent: bool,
    #[pyo3(get, set)]
    pub basis_points: f64,
    #[pyo3(get, set)]
    pub basis_points_type: i32,
    #[pyo3(get, set)]
    pub block_order: bool,
    #[pyo3(get, set)]
    pub bond_accrued_interest: String,
    #[pyo3(get, set)]
    pub clearing_account: String,
    #[pyo3(get, set)]
    pub clearing_intent: String,
    #[pyo3(get, set)]
    pub client_id: i32,
    #[pyo3(get, set)]
    pub compete_against_best_offset: f64,
    #[pyo3(get, set)]
    pub continuous_update: bool,
    #[pyo3(get, set)]
    pub customer_account: String,
    #[pyo3(get, set)]
    pub deactivate: bool,
    #[pyo3(get, set)]
    pub delta: f64,
    #[pyo3(get, set)]
    pub delta_neutral_aux_price: f64,
    #[pyo3(get, set)]
    pub delta_neutral_clearing_account: String,
    #[pyo3(get, set)]
    pub delta_neutral_clearing_intent: String,
    #[pyo3(get, set)]
    pub delta_neutral_con_id: i32,
    #[pyo3(get, set)]
    pub delta_neutral_designated_location: String,
    #[pyo3(get, set)]
    pub delta_neutral_open_close: String,
    #[pyo3(get, set)]
    pub delta_neutral_order_type: String,
    #[pyo3(get, set)]
    pub delta_neutral_settling_firm: String,
    #[pyo3(get, set)]
    pub delta_neutral_short_sale: bool,
    #[pyo3(get, set)]
    pub delta_neutral_short_sale_slot: i32,
    #[pyo3(get, set)]
    pub designated_location: String,
    #[pyo3(get, set)]
    pub discretionary_up_to_limit_price: bool,
    #[pyo3(get, set)]
    pub dont_use_auto_price_for_hedge: bool,
    #[pyo3(get, set)]
    pub duration: i32,
    #[pyo3(get, set)]
    pub exempt_code: i32,
    #[pyo3(get, set)]
    pub ext_operator: String,
    #[pyo3(get, set)]
    pub fa_group: String,
    #[pyo3(get, set)]
    pub fa_method: String,
    #[pyo3(get, set)]
    pub fa_percentage: String,
    /// The official API's Decimal, as `total_quantity`.
    pub filled_quantity: f64,
    #[pyo3(get, set)]
    pub hedge_max_size: i32,
    #[pyo3(get, set)]
    pub hedge_param: String,
    #[pyo3(get, set)]
    pub hedge_type: String,
    #[pyo3(get, set)]
    pub ignore_open_auction: bool,
    #[pyo3(get, set)]
    pub imbalance_only: bool,
    #[pyo3(get, set)]
    pub include_overnight: bool,
    #[pyo3(get, set)]
    pub is_oms_container: bool,
    #[pyo3(get, set)]
    pub is_pegged_change_amount_decrease: bool,
    #[pyo3(get, set)]
    pub lmt_price_offset: f64,
    #[pyo3(get, set)]
    pub manual_order_indicator: i32,
    #[pyo3(get, set)]
    pub manual_order_time: String,
    #[pyo3(get, set)]
    pub mid_offset_at_half: f64,
    #[pyo3(get, set)]
    pub mid_offset_at_whole: f64,
    #[pyo3(get, set)]
    pub mifid2_decision_algo: String,
    #[pyo3(get, set)]
    pub mifid2_decision_maker: String,
    #[pyo3(get, set)]
    pub mifid2_execution_algo: String,
    #[pyo3(get, set)]
    pub mifid2_execution_trader: String,
    #[pyo3(get, set)]
    pub min_compete_size: i32,
    #[pyo3(get, set)]
    pub min_trade_qty: i32,
    #[pyo3(get, set)]
    pub model_code: String,
    #[pyo3(get, set)]
    pub not_held: bool,
    #[pyo3(get, set)]
    pub oca_type: i32,
    #[pyo3(get, set)]
    pub open_close: String,
    #[pyo3(get, set)]
    pub opt_out_smart_routing: bool,
    #[pyo3(get, set)]
    pub order_combo_legs: Py<PyAny>,
    #[pyo3(get, set)]
    pub order_misc_options: Py<PyAny>,
    #[pyo3(get, set)]
    pub order_ref: String,
    #[pyo3(get, set)]
    pub origin: i32,
    #[pyo3(get, set)]
    pub override_percentage_constraints: bool,
    #[pyo3(get, set)]
    pub parent_perm_id: i64,
    #[pyo3(get, set)]
    pub pegged_change_amount: f64,
    #[pyo3(get, set)]
    pub percent_offset: f64,
    #[pyo3(get, set)]
    pub perm_id: i64,
    #[pyo3(get, set)]
    pub post_only: bool,
    #[pyo3(get, set)]
    pub post_to_ats: i32,
    #[pyo3(get, set)]
    pub professional_customer: bool,
    #[pyo3(get, set)]
    pub pt_order_id: i32,
    #[pyo3(get, set)]
    pub pt_order_type: String,
    #[pyo3(get, set)]
    pub randomize_price: bool,
    #[pyo3(get, set)]
    pub randomize_size: bool,
    #[pyo3(get, set)]
    pub ref_futures_con_id: i32,
    #[pyo3(get, set)]
    pub reference_change_amount: f64,
    #[pyo3(get, set)]
    pub reference_contract_id: i32,
    #[pyo3(get, set)]
    pub reference_exchange_id: String,
    #[pyo3(get, set)]
    pub reference_price_type: i32,
    /// None when unset, as the official API's.
    #[pyo3(get, set)]
    pub route_marketable_to_bbo: Option<bool>,
    #[pyo3(get, set)]
    pub rule80a: String,
    #[pyo3(get, set)]
    pub scale_auto_reset: bool,
    #[pyo3(get, set)]
    pub scale_init_fill_qty: i32,
    #[pyo3(get, set)]
    pub scale_init_level_size: i32,
    #[pyo3(get, set)]
    pub scale_init_position: i32,
    #[pyo3(get, set)]
    pub scale_price_adjust_interval: i32,
    #[pyo3(get, set)]
    pub scale_price_adjust_value: f64,
    #[pyo3(get, set)]
    pub scale_price_increment: f64,
    #[pyo3(get, set)]
    pub scale_profit_offset: f64,
    #[pyo3(get, set)]
    pub scale_random_percent: bool,
    #[pyo3(get, set)]
    pub scale_subs_level_size: i32,
    #[pyo3(get, set)]
    pub scale_table: String,
    /// None when unset, as the official API's.
    #[pyo3(get, set)]
    pub seek_price_improvement: Option<bool>,
    #[pyo3(get, set)]
    pub settling_firm: String,
    #[pyo3(get, set)]
    pub shareholder: String,
    #[pyo3(get, set)]
    pub short_sale_slot: i32,
    #[pyo3(get, set)]
    pub sl_order_id: i32,
    #[pyo3(get, set)]
    pub sl_order_type: String,
    #[pyo3(get, set)]
    pub smart_combo_routing_params: Py<PyAny>,
    #[pyo3(get, set)]
    pub soft_dollar_tier_name: String,
    #[pyo3(get, set)]
    pub soft_dollar_tier_val: String,
    #[pyo3(get, set)]
    pub soft_dollar_tier_display_name: String,
    #[pyo3(get, set)]
    pub solicited: bool,
    #[pyo3(get, set)]
    pub starting_price: f64,
    #[pyo3(get, set)]
    pub stock_range_lower: f64,
    #[pyo3(get, set)]
    pub stock_range_upper: f64,
    #[pyo3(get, set)]
    pub stock_ref_price: f64,
    #[pyo3(get, set)]
    pub submitter: String,
    #[pyo3(get, set)]
    pub trail_stop_price: f64,
    /// None when unset, as the API (ibx#492).
    #[pyo3(get, set)]
    pub use_price_mgmt_algo: Option<bool>,
    #[pyo3(get, set)]
    pub volatility: f64,
    #[pyo3(get, set)]
    pub volatility_type: i32,
    #[pyo3(get, set)]
    pub what_if_type: i32,
}

impl Clone for Order {
    fn clone(&self) -> Self {
        Self {
            // Original fields
            order_id: self.order_id,
            action: self.action.clone(),
            total_quantity: self.total_quantity,
            order_type: self.order_type.clone(),
            lmt_price: self.lmt_price,
            aux_price: self.aux_price,
            tif: self.tif.clone(),
            outside_rth: self.outside_rth,
            display_size: self.display_size,
            min_qty: self.min_qty,
            hidden: self.hidden,
            good_after_time: self.good_after_time.clone(),
            good_till_date: self.good_till_date.clone(),
            oca_group: self.oca_group.clone(),
            trailing_percent: self.trailing_percent,
            algo_strategy: self.algo_strategy.clone(),
            algo_params: Python::attach(|py| self.algo_params.clone_ref(py)),
            what_if: self.what_if,
            cash_qty: self.cash_qty,
            parent_id: self.parent_id,
            transmit: self.transmit,
            discretionary_amt: self.discretionary_amt,
            sweep_to_fill: self.sweep_to_fill,
            all_or_none: self.all_or_none,
            trigger_method: self.trigger_method,
            adjusted_order_type: self.adjusted_order_type.clone(),
            trigger_price: self.trigger_price,
            adjusted_stop_price: self.adjusted_stop_price,
            adjusted_stop_limit_price: self.adjusted_stop_limit_price,
            conditions: Python::attach(|py| self.conditions.clone_ref(py)),
            conditions_ignore_rth: self.conditions_ignore_rth,
            conditions_cancel_order: self.conditions_cancel_order,
            // New fields
            account: self.account.clone(),
            active_start_time: self.active_start_time.clone(),
            active_stop_time: self.active_stop_time.clone(),
            adjustable_trailing_unit: self.adjustable_trailing_unit,
            adjusted_trailing_amount: self.adjusted_trailing_amount,
            advanced_error_override: self.advanced_error_override.clone(),
            algo_id: self.algo_id.clone(),
            allow_pre_open: self.allow_pre_open,
            auction_strategy: self.auction_strategy,
            auto_cancel_date: self.auto_cancel_date.clone(),
            auto_cancel_parent: self.auto_cancel_parent,
            basis_points: self.basis_points,
            basis_points_type: self.basis_points_type,
            block_order: self.block_order,
            bond_accrued_interest: self.bond_accrued_interest.clone(),
            clearing_account: self.clearing_account.clone(),
            clearing_intent: self.clearing_intent.clone(),
            client_id: self.client_id,
            compete_against_best_offset: self.compete_against_best_offset,
            continuous_update: self.continuous_update,
            customer_account: self.customer_account.clone(),
            deactivate: self.deactivate,
            delta: self.delta,
            delta_neutral_aux_price: self.delta_neutral_aux_price,
            delta_neutral_clearing_account: self.delta_neutral_clearing_account.clone(),
            delta_neutral_clearing_intent: self.delta_neutral_clearing_intent.clone(),
            delta_neutral_con_id: self.delta_neutral_con_id,
            delta_neutral_designated_location: self.delta_neutral_designated_location.clone(),
            delta_neutral_open_close: self.delta_neutral_open_close.clone(),
            delta_neutral_order_type: self.delta_neutral_order_type.clone(),
            delta_neutral_settling_firm: self.delta_neutral_settling_firm.clone(),
            delta_neutral_short_sale: self.delta_neutral_short_sale,
            delta_neutral_short_sale_slot: self.delta_neutral_short_sale_slot,
            designated_location: self.designated_location.clone(),
            discretionary_up_to_limit_price: self.discretionary_up_to_limit_price,
            dont_use_auto_price_for_hedge: self.dont_use_auto_price_for_hedge,
            duration: self.duration,
            exempt_code: self.exempt_code,
            ext_operator: self.ext_operator.clone(),
            fa_group: self.fa_group.clone(),
            fa_method: self.fa_method.clone(),
            fa_percentage: self.fa_percentage.clone(),
            filled_quantity: self.filled_quantity,
            hedge_max_size: self.hedge_max_size,
            hedge_param: self.hedge_param.clone(),
            hedge_type: self.hedge_type.clone(),
            ignore_open_auction: self.ignore_open_auction,
            imbalance_only: self.imbalance_only,
            include_overnight: self.include_overnight,
            is_oms_container: self.is_oms_container,
            is_pegged_change_amount_decrease: self.is_pegged_change_amount_decrease,
            lmt_price_offset: self.lmt_price_offset,
            manual_order_indicator: self.manual_order_indicator,
            manual_order_time: self.manual_order_time.clone(),
            mid_offset_at_half: self.mid_offset_at_half,
            mid_offset_at_whole: self.mid_offset_at_whole,
            mifid2_decision_algo: self.mifid2_decision_algo.clone(),
            mifid2_decision_maker: self.mifid2_decision_maker.clone(),
            mifid2_execution_algo: self.mifid2_execution_algo.clone(),
            mifid2_execution_trader: self.mifid2_execution_trader.clone(),
            min_compete_size: self.min_compete_size,
            min_trade_qty: self.min_trade_qty,
            model_code: self.model_code.clone(),
            not_held: self.not_held,
            oca_type: self.oca_type,
            open_close: self.open_close.clone(),
            opt_out_smart_routing: self.opt_out_smart_routing,
            // The per-leg prices of a combo are kept (ibx#470).
            order_combo_legs: Python::attach(|py| self.order_combo_legs.clone_ref(py)),
            order_misc_options: Python::attach(|py| self.order_misc_options.clone_ref(py)),
            order_ref: self.order_ref.clone(),
            origin: self.origin,
            override_percentage_constraints: self.override_percentage_constraints,
            parent_perm_id: self.parent_perm_id,
            pegged_change_amount: self.pegged_change_amount,
            percent_offset: self.percent_offset,
            perm_id: self.perm_id,
            post_only: self.post_only,
            post_to_ats: self.post_to_ats,
            professional_customer: self.professional_customer,
            pt_order_id: self.pt_order_id,
            pt_order_type: self.pt_order_type.clone(),
            randomize_price: self.randomize_price,
            randomize_size: self.randomize_size,
            ref_futures_con_id: self.ref_futures_con_id,
            reference_change_amount: self.reference_change_amount,
            reference_contract_id: self.reference_contract_id,
            reference_exchange_id: self.reference_exchange_id.clone(),
            reference_price_type: self.reference_price_type,
            route_marketable_to_bbo: self.route_marketable_to_bbo,
            rule80a: self.rule80a.clone(),
            scale_auto_reset: self.scale_auto_reset,
            scale_init_fill_qty: self.scale_init_fill_qty,
            scale_init_level_size: self.scale_init_level_size,
            scale_init_position: self.scale_init_position,
            scale_price_adjust_interval: self.scale_price_adjust_interval,
            scale_price_adjust_value: self.scale_price_adjust_value,
            scale_price_increment: self.scale_price_increment,
            scale_profit_offset: self.scale_profit_offset,
            scale_random_percent: self.scale_random_percent,
            scale_subs_level_size: self.scale_subs_level_size,
            scale_table: self.scale_table.clone(),
            seek_price_improvement: self.seek_price_improvement,
            settling_firm: self.settling_firm.clone(),
            shareholder: self.shareholder.clone(),
            short_sale_slot: self.short_sale_slot,
            sl_order_id: self.sl_order_id,
            sl_order_type: self.sl_order_type.clone(),
            smart_combo_routing_params: Python::attach(|py| self.smart_combo_routing_params.clone_ref(py)),
            soft_dollar_tier_name: self.soft_dollar_tier_name.clone(),
            soft_dollar_tier_val: self.soft_dollar_tier_val.clone(),
            soft_dollar_tier_display_name: self.soft_dollar_tier_display_name.clone(),
            solicited: self.solicited,
            starting_price: self.starting_price,
            stock_range_lower: self.stock_range_lower,
            stock_range_upper: self.stock_range_upper,
            stock_ref_price: self.stock_ref_price,
            submitter: self.submitter.clone(),
            trail_stop_price: self.trail_stop_price,
            use_price_mgmt_algo: self.use_price_mgmt_algo,
            volatility: self.volatility,
            volatility_type: self.volatility_type,
            what_if_type: self.what_if_type,
        }
    }
}

/// The official API's defaults (ibapi 10.46 `Order()`): an unset double is
/// `f64::MAX`, an unset int `i32::MAX`, an unset Decimal `f64::MAX` here,
/// empty texts, None for the optional lists and flags.
impl Default for Order {
    fn default() -> Self {
        let none = || Python::attach(|py| py.None());
        Self {
            // Original fields
            order_id: 0,
            action: String::new(),
            total_quantity: f64::MAX,
            order_type: String::new(),
            lmt_price: f64::MAX,
            aux_price: f64::MAX,
            tif: String::new(),
            outside_rth: false,
            display_size: 0,
            min_qty: i32::MAX,
            hidden: false,
            good_after_time: String::new(),
            good_till_date: String::new(),
            oca_group: String::new(),
            trailing_percent: f64::MAX,
            algo_strategy: String::new(),
            algo_params: none(),
            what_if: false,
            cash_qty: f64::MAX,
            parent_id: 0,
            transmit: true,
            discretionary_amt: 0.0,
            sweep_to_fill: false,
            all_or_none: false,
            trigger_method: 0,
            adjusted_order_type: String::new(),
            trigger_price: f64::MAX,
            adjusted_stop_price: f64::MAX,
            adjusted_stop_limit_price: f64::MAX,
            conditions: empty_list(),
            conditions_ignore_rth: false,
            conditions_cancel_order: false,
            // New fields
            account: String::new(),
            active_start_time: String::new(),
            active_stop_time: String::new(),
            adjustable_trailing_unit: 0,
            adjusted_trailing_amount: f64::MAX,
            advanced_error_override: String::new(),
            algo_id: String::new(),
            allow_pre_open: false,
            auction_strategy: 0,
            auto_cancel_date: String::new(),
            auto_cancel_parent: false,
            basis_points: f64::MAX,
            basis_points_type: i32::MAX,
            block_order: false,
            bond_accrued_interest: String::new(),
            clearing_account: String::new(),
            clearing_intent: String::new(),
            client_id: 0,
            compete_against_best_offset: f64::MAX,
            continuous_update: false,
            customer_account: String::new(),
            deactivate: false,
            delta: f64::MAX,
            delta_neutral_aux_price: f64::MAX,
            delta_neutral_clearing_account: String::new(),
            delta_neutral_clearing_intent: String::new(),
            delta_neutral_con_id: 0,
            delta_neutral_designated_location: String::new(),
            delta_neutral_open_close: String::new(),
            delta_neutral_order_type: String::new(),
            delta_neutral_settling_firm: String::new(),
            delta_neutral_short_sale: false,
            delta_neutral_short_sale_slot: 0,
            designated_location: String::new(),
            discretionary_up_to_limit_price: false,
            dont_use_auto_price_for_hedge: false,
            duration: i32::MAX,
            exempt_code: -1,
            ext_operator: String::new(),
            fa_group: String::new(),
            fa_method: String::new(),
            fa_percentage: String::new(),
            filled_quantity: f64::MAX,
            hedge_max_size: i32::MAX,
            hedge_param: String::new(),
            hedge_type: String::new(),
            ignore_open_auction: false,
            imbalance_only: false,
            include_overnight: false,
            is_oms_container: false,
            is_pegged_change_amount_decrease: false,
            lmt_price_offset: f64::MAX,
            manual_order_indicator: i32::MAX,
            manual_order_time: String::new(),
            mid_offset_at_half: f64::MAX,
            mid_offset_at_whole: f64::MAX,
            mifid2_decision_algo: String::new(),
            mifid2_decision_maker: String::new(),
            mifid2_execution_algo: String::new(),
            mifid2_execution_trader: String::new(),
            min_compete_size: i32::MAX,
            min_trade_qty: i32::MAX,
            model_code: String::new(),
            not_held: false,
            oca_type: 0,
            open_close: String::new(),
            opt_out_smart_routing: false,
            order_combo_legs: none(),
            order_misc_options: none(),
            order_ref: String::new(),
            origin: 0,
            override_percentage_constraints: false,
            parent_perm_id: 0,
            pegged_change_amount: 0.0,
            percent_offset: f64::MAX,
            perm_id: 0,
            post_only: false,
            post_to_ats: i32::MAX,
            professional_customer: false,
            pt_order_id: i32::MAX,
            pt_order_type: String::new(),
            randomize_price: false,
            randomize_size: false,
            ref_futures_con_id: 0,
            reference_change_amount: 0.0,
            reference_contract_id: 0,
            reference_exchange_id: String::new(),
            reference_price_type: i32::MAX,
            route_marketable_to_bbo: None,
            rule80a: String::new(),
            scale_auto_reset: false,
            scale_init_fill_qty: i32::MAX,
            scale_init_level_size: i32::MAX,
            scale_init_position: i32::MAX,
            scale_price_adjust_interval: i32::MAX,
            scale_price_adjust_value: f64::MAX,
            scale_price_increment: f64::MAX,
            scale_profit_offset: f64::MAX,
            scale_random_percent: false,
            scale_subs_level_size: i32::MAX,
            scale_table: String::new(),
            seek_price_improvement: None,
            settling_firm: String::new(),
            shareholder: String::new(),
            short_sale_slot: 0,
            sl_order_id: i32::MAX,
            sl_order_type: String::new(),
            smart_combo_routing_params: none(),
            soft_dollar_tier_name: String::new(),
            soft_dollar_tier_val: String::new(),
            soft_dollar_tier_display_name: String::new(),
            solicited: false,
            starting_price: f64::MAX,
            stock_range_lower: f64::MAX,
            stock_range_upper: f64::MAX,
            stock_ref_price: f64::MAX,
            submitter: String::new(),
            trail_stop_price: f64::MAX,
            use_price_mgmt_algo: None,
            volatility: f64::MAX,
            volatility_type: i32::MAX,
            what_if_type: i32::MAX,
        }
    }
}

#[pymethods]
impl Order {
    #[new]
    #[pyo3(signature = (
        order_id=0, action="".to_string(), total_quantity=None, order_type="".to_string(),
        lmt_price=f64::MAX, aux_price=f64::MAX, tif="".to_string(), outside_rth=false,
        display_size=0, min_qty=i32::MAX, hidden=false, good_after_time="".to_string(),
        good_till_date="".to_string(), oca_group="".to_string(), trailing_percent=f64::MAX,
        algo_strategy="".to_string(), what_if=false, cash_qty=f64::MAX, parent_id=0,
        transmit=true
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        order_id: i64,
        action: String,
        total_quantity: Option<&Bound<'_, PyAny>>,
        order_type: String,
        lmt_price: f64,
        aux_price: f64,
        tif: String,
        outside_rth: bool,
        display_size: i32,
        min_qty: i32,
        hidden: bool,
        good_after_time: String,
        good_till_date: String,
        oca_group: String,
        trailing_percent: f64,
        algo_strategy: String,
        what_if: bool,
        cash_qty: f64,
        parent_id: i64,
        transmit: bool,
    ) -> PyResult<Self> {
        Ok(Self {
            order_id,
            action,
            total_quantity: total_quantity.map(decimal_from_py).transpose()?.unwrap_or(f64::MAX),
            order_type,
            lmt_price,
            aux_price,
            tif,
            outside_rth,
            display_size,
            min_qty,
            hidden,
            good_after_time,
            good_till_date,
            oca_group,
            trailing_percent,
            algo_strategy,
            what_if,
            cash_qty,
            parent_id,
            transmit,
            ..Default::default()
        })
    }

    fn __repr__(&self) -> String {
        format!("Order(orderId={}, action='{}', totalQuantity={}, orderType='{}', lmtPrice={}, auxPrice={})",
            self.order_id, self.action, self.total_quantity, self.order_type, self.lmt_price, self.aux_price)
    }

    // ── Existing camelCase aliases ──
    #[getter(auxPrice)]
    fn get_aux_price_alias(&self) -> f64 { self.aux_price }
    #[setter(auxPrice)]
    fn set_aux_price_alias(&mut self, v: f64) { self.aux_price = v; }
    #[getter(lmtPrice)]
    fn get_lmt_price_alias(&self) -> f64 { self.lmt_price }
    #[setter(lmtPrice)]
    fn set_lmt_price_alias(&mut self, v: f64) { self.lmt_price = v; }
    #[getter(orderId)]
    fn get_order_id_alias(&self) -> i64 { self.order_id }
    #[setter(orderId)]
    fn set_order_id_alias(&mut self, v: i64) { self.order_id = v; }
    // The official API's Decimals (ibapi 10.46): a `decimal.Decimal`,
    // unset `UNSET_DECIMAL`; set from a Decimal, an int, a float or a text.
    #[getter(totalQuantity)]
    fn get_total_quantity_alias(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.total_quantity) }
    #[setter(totalQuantity)]
    fn set_total_quantity_alias(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.total_quantity = decimal_from_py(v)?; Ok(()) }
    #[getter(total_quantity)]
    fn get_total_quantity(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.total_quantity) }
    #[setter(total_quantity)]
    fn set_total_quantity(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.total_quantity = decimal_from_py(v)?; Ok(()) }
    #[getter(filledQuantity)]
    fn get_filled_quantity_alias(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.filled_quantity) }
    #[setter(filledQuantity)]
    fn set_filled_quantity_alias(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.filled_quantity = decimal_from_py(v)?; Ok(()) }
    #[getter(filled_quantity)]
    fn get_filled_quantity(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.filled_quantity) }
    #[setter(filled_quantity)]
    fn set_filled_quantity(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.filled_quantity = decimal_from_py(v)?; Ok(()) }
    #[getter(hedgeMaxSize)]
    fn get_hedge_max_size_alias(&self) -> i32 { self.hedge_max_size }
    #[setter(hedgeMaxSize)]
    fn set_hedge_max_size_alias(&mut self, v: i32) { self.hedge_max_size = v; }
    #[getter(orderType)]
    fn get_order_type_alias(&self) -> String { self.order_type.clone() }
    #[setter(orderType)]
    fn set_order_type_alias(&mut self, v: String) { self.order_type = v; }

    // ── New camelCase aliases ──
    #[getter(activeStartTime)]
    fn get_active_start_time_alias(&self) -> String { self.active_start_time.clone() }
    #[setter(activeStartTime)]
    fn set_active_start_time_alias(&mut self, v: String) { self.active_start_time = v; }
    #[getter(activeStopTime)]
    fn get_active_stop_time_alias(&self) -> String { self.active_stop_time.clone() }
    #[setter(activeStopTime)]
    fn set_active_stop_time_alias(&mut self, v: String) { self.active_stop_time = v; }
    #[getter(adjustableTrailingUnit)]
    fn get_adjustable_trailing_unit_alias(&self) -> i32 { self.adjustable_trailing_unit }
    #[setter(adjustableTrailingUnit)]
    fn set_adjustable_trailing_unit_alias(&mut self, v: i32) { self.adjustable_trailing_unit = v; }
    #[getter(adjustedTrailingAmount)]
    fn get_adjusted_trailing_amount_alias(&self) -> f64 { self.adjusted_trailing_amount }
    #[setter(adjustedTrailingAmount)]
    fn set_adjusted_trailing_amount_alias(&mut self, v: f64) { self.adjusted_trailing_amount = v; }
    #[getter(adjustedOrderType)]
    fn get_adjusted_order_type_alias(&self) -> String { self.adjusted_order_type.clone() }
    #[setter(adjustedOrderType)]
    fn set_adjusted_order_type_alias(&mut self, v: String) { self.adjusted_order_type = v; }
    #[getter(adjustedStopPrice)]
    fn get_adjusted_stop_price_alias(&self) -> f64 { self.adjusted_stop_price }
    #[setter(adjustedStopPrice)]
    fn set_adjusted_stop_price_alias(&mut self, v: f64) { self.adjusted_stop_price = v; }
    #[getter(adjustedStopLimitPrice)]
    fn get_adjusted_stop_limit_price_alias(&self) -> f64 { self.adjusted_stop_limit_price }
    #[setter(adjustedStopLimitPrice)]
    fn set_adjusted_stop_limit_price_alias(&mut self, v: f64) { self.adjusted_stop_limit_price = v; }
    #[getter(advancedErrorOverride)]
    fn get_advanced_error_override_alias(&self) -> String { self.advanced_error_override.clone() }
    #[setter(advancedErrorOverride)]
    fn set_advanced_error_override_alias(&mut self, v: String) { self.advanced_error_override = v; }
    #[getter(algoId)]
    fn get_algo_id_alias(&self) -> String { self.algo_id.clone() }
    #[setter(algoId)]
    fn set_algo_id_alias(&mut self, v: String) { self.algo_id = v; }
    #[getter(algoParams)]
    fn get_algo_params_alias(&self, py: Python<'_>) -> Py<PyAny> { self.algo_params.clone_ref(py) }
    #[setter(algoParams)]
    fn set_algo_params_alias(&mut self, v: Py<PyAny>) { self.algo_params = v; }
    #[getter(algoStrategy)]
    fn get_algo_strategy_alias(&self) -> String { self.algo_strategy.clone() }
    #[setter(algoStrategy)]
    fn set_algo_strategy_alias(&mut self, v: String) { self.algo_strategy = v; }
    #[getter(allOrNone)]
    fn get_all_or_none_alias(&self) -> bool { self.all_or_none }
    #[setter(allOrNone)]
    fn set_all_or_none_alias(&mut self, v: bool) { self.all_or_none = v; }
    #[getter(allowPreOpen)]
    fn get_allow_pre_open_alias(&self) -> bool { self.allow_pre_open }
    #[setter(allowPreOpen)]
    fn set_allow_pre_open_alias(&mut self, v: bool) { self.allow_pre_open = v; }
    #[getter(auctionStrategy)]
    fn get_auction_strategy_alias(&self) -> i32 { self.auction_strategy }
    #[setter(auctionStrategy)]
    fn set_auction_strategy_alias(&mut self, v: i32) { self.auction_strategy = v; }
    #[getter(autoCancelDate)]
    fn get_auto_cancel_date_alias(&self) -> String { self.auto_cancel_date.clone() }
    #[setter(autoCancelDate)]
    fn set_auto_cancel_date_alias(&mut self, v: String) { self.auto_cancel_date = v; }
    #[getter(autoCancelParent)]
    fn get_auto_cancel_parent_alias(&self) -> bool { self.auto_cancel_parent }
    #[setter(autoCancelParent)]
    fn set_auto_cancel_parent_alias(&mut self, v: bool) { self.auto_cancel_parent = v; }
    #[getter(basisPoints)]
    fn get_basis_points_alias(&self) -> f64 { self.basis_points }
    #[setter(basisPoints)]
    fn set_basis_points_alias(&mut self, v: f64) { self.basis_points = v; }
    #[getter(basisPointsType)]
    fn get_basis_points_type_alias(&self) -> i32 { self.basis_points_type }
    #[setter(basisPointsType)]
    fn set_basis_points_type_alias(&mut self, v: i32) { self.basis_points_type = v; }
    #[getter(blockOrder)]
    fn get_block_order_alias(&self) -> bool { self.block_order }
    #[setter(blockOrder)]
    fn set_block_order_alias(&mut self, v: bool) { self.block_order = v; }
    #[getter(bondAccruedInterest)]
    fn get_bond_accrued_interest_alias(&self) -> String { self.bond_accrued_interest.clone() }
    #[setter(bondAccruedInterest)]
    fn set_bond_accrued_interest_alias(&mut self, v: String) { self.bond_accrued_interest = v; }
    #[getter(cashQty)]
    fn get_cash_qty_alias(&self) -> f64 { self.cash_qty }
    #[setter(cashQty)]
    fn set_cash_qty_alias(&mut self, v: f64) { self.cash_qty = v; }
    #[getter(clearingAccount)]
    fn get_clearing_account_alias(&self) -> String { self.clearing_account.clone() }
    #[setter(clearingAccount)]
    fn set_clearing_account_alias(&mut self, v: String) { self.clearing_account = v; }
    #[getter(clearingIntent)]
    fn get_clearing_intent_alias(&self) -> String { self.clearing_intent.clone() }
    #[setter(clearingIntent)]
    fn set_clearing_intent_alias(&mut self, v: String) { self.clearing_intent = v; }
    #[getter(clientId)]
    fn get_client_id_alias(&self) -> i32 { self.client_id }
    #[setter(clientId)]
    fn set_client_id_alias(&mut self, v: i32) { self.client_id = v; }
    #[getter(competeAgainstBestOffset)]
    fn get_compete_against_best_offset_alias(&self) -> f64 { self.compete_against_best_offset }
    #[setter(competeAgainstBestOffset)]
    fn set_compete_against_best_offset_alias(&mut self, v: f64) { self.compete_against_best_offset = v; }
    #[getter(conditionsCancelOrder)]
    fn get_conditions_cancel_order_alias(&self) -> bool { self.conditions_cancel_order }
    #[setter(conditionsCancelOrder)]
    fn set_conditions_cancel_order_alias(&mut self, v: bool) { self.conditions_cancel_order = v; }
    #[getter(conditionsIgnoreRth)]
    fn get_conditions_ignore_rth_alias(&self) -> bool { self.conditions_ignore_rth }
    #[setter(conditionsIgnoreRth)]
    fn set_conditions_ignore_rth_alias(&mut self, v: bool) { self.conditions_ignore_rth = v; }
    #[getter(continuousUpdate)]
    fn get_continuous_update_alias(&self) -> bool { self.continuous_update }
    #[setter(continuousUpdate)]
    fn set_continuous_update_alias(&mut self, v: bool) { self.continuous_update = v; }
    #[getter(customerAccount)]
    fn get_customer_account_alias(&self) -> String { self.customer_account.clone() }
    #[setter(customerAccount)]
    fn set_customer_account_alias(&mut self, v: String) { self.customer_account = v; }
    #[getter(deltaNeutralAuxPrice)]
    fn get_delta_neutral_aux_price_alias(&self) -> f64 { self.delta_neutral_aux_price }
    #[setter(deltaNeutralAuxPrice)]
    fn set_delta_neutral_aux_price_alias(&mut self, v: f64) { self.delta_neutral_aux_price = v; }
    #[getter(deltaNeutralClearingAccount)]
    fn get_delta_neutral_clearing_account_alias(&self) -> String { self.delta_neutral_clearing_account.clone() }
    #[setter(deltaNeutralClearingAccount)]
    fn set_delta_neutral_clearing_account_alias(&mut self, v: String) { self.delta_neutral_clearing_account = v; }
    #[getter(deltaNeutralClearingIntent)]
    fn get_delta_neutral_clearing_intent_alias(&self) -> String { self.delta_neutral_clearing_intent.clone() }
    #[setter(deltaNeutralClearingIntent)]
    fn set_delta_neutral_clearing_intent_alias(&mut self, v: String) { self.delta_neutral_clearing_intent = v; }
    #[getter(deltaNeutralConId)]
    fn get_delta_neutral_con_id_alias(&self) -> i32 { self.delta_neutral_con_id }
    #[setter(deltaNeutralConId)]
    fn set_delta_neutral_con_id_alias(&mut self, v: i32) { self.delta_neutral_con_id = v; }
    #[getter(deltaNeutralDesignatedLocation)]
    fn get_delta_neutral_designated_location_alias(&self) -> String { self.delta_neutral_designated_location.clone() }
    #[setter(deltaNeutralDesignatedLocation)]
    fn set_delta_neutral_designated_location_alias(&mut self, v: String) { self.delta_neutral_designated_location = v; }
    #[getter(deltaNeutralOpenClose)]
    fn get_delta_neutral_open_close_alias(&self) -> String { self.delta_neutral_open_close.clone() }
    #[setter(deltaNeutralOpenClose)]
    fn set_delta_neutral_open_close_alias(&mut self, v: String) { self.delta_neutral_open_close = v; }
    #[getter(deltaNeutralOrderType)]
    fn get_delta_neutral_order_type_alias(&self) -> String { self.delta_neutral_order_type.clone() }
    #[setter(deltaNeutralOrderType)]
    fn set_delta_neutral_order_type_alias(&mut self, v: String) { self.delta_neutral_order_type = v; }
    #[getter(deltaNeutralSettlingFirm)]
    fn get_delta_neutral_settling_firm_alias(&self) -> String { self.delta_neutral_settling_firm.clone() }
    #[setter(deltaNeutralSettlingFirm)]
    fn set_delta_neutral_settling_firm_alias(&mut self, v: String) { self.delta_neutral_settling_firm = v; }
    #[getter(deltaNeutralShortSale)]
    fn get_delta_neutral_short_sale_alias(&self) -> bool { self.delta_neutral_short_sale }
    #[setter(deltaNeutralShortSale)]
    fn set_delta_neutral_short_sale_alias(&mut self, v: bool) { self.delta_neutral_short_sale = v; }
    #[getter(deltaNeutralShortSaleSlot)]
    fn get_delta_neutral_short_sale_slot_alias(&self) -> i32 { self.delta_neutral_short_sale_slot }
    #[setter(deltaNeutralShortSaleSlot)]
    fn set_delta_neutral_short_sale_slot_alias(&mut self, v: i32) { self.delta_neutral_short_sale_slot = v; }
    #[getter(designatedLocation)]
    fn get_designated_location_alias(&self) -> String { self.designated_location.clone() }
    #[setter(designatedLocation)]
    fn set_designated_location_alias(&mut self, v: String) { self.designated_location = v; }
    #[getter(discretionaryAmt)]
    fn get_discretionary_amt_alias(&self) -> f64 { self.discretionary_amt }
    #[setter(discretionaryAmt)]
    fn set_discretionary_amt_alias(&mut self, v: f64) { self.discretionary_amt = v; }
    #[getter(discretionaryUpToLimitPrice)]
    fn get_discretionary_up_to_limit_price_alias(&self) -> bool { self.discretionary_up_to_limit_price }
    #[setter(discretionaryUpToLimitPrice)]
    fn set_discretionary_up_to_limit_price_alias(&mut self, v: bool) { self.discretionary_up_to_limit_price = v; }
    #[getter(displaySize)]
    fn get_display_size_alias(&self) -> i32 { self.display_size }
    #[setter(displaySize)]
    fn set_display_size_alias(&mut self, v: i32) { self.display_size = v; }
    #[getter(dontUseAutoPriceForHedge)]
    fn get_dont_use_auto_price_for_hedge_alias(&self) -> bool { self.dont_use_auto_price_for_hedge }
    #[setter(dontUseAutoPriceForHedge)]
    fn set_dont_use_auto_price_for_hedge_alias(&mut self, v: bool) { self.dont_use_auto_price_for_hedge = v; }
    #[getter(exemptCode)]
    fn get_exempt_code_alias(&self) -> i32 { self.exempt_code }
    #[setter(exemptCode)]
    fn set_exempt_code_alias(&mut self, v: i32) { self.exempt_code = v; }
    #[getter(extOperator)]
    fn get_ext_operator_alias(&self) -> String { self.ext_operator.clone() }
    #[setter(extOperator)]
    fn set_ext_operator_alias(&mut self, v: String) { self.ext_operator = v; }
    #[getter(faGroup)]
    fn get_fa_group_alias(&self) -> String { self.fa_group.clone() }
    #[setter(faGroup)]
    fn set_fa_group_alias(&mut self, v: String) { self.fa_group = v; }
    #[getter(faMethod)]
    fn get_fa_method_alias(&self) -> String { self.fa_method.clone() }
    #[setter(faMethod)]
    fn set_fa_method_alias(&mut self, v: String) { self.fa_method = v; }
    #[getter(faPercentage)]
    fn get_fa_percentage_alias(&self) -> String { self.fa_percentage.clone() }
    #[setter(faPercentage)]
    fn set_fa_percentage_alias(&mut self, v: String) { self.fa_percentage = v; }
    #[getter(goodAfterTime)]
    fn get_good_after_time_alias(&self) -> String { self.good_after_time.clone() }
    #[setter(goodAfterTime)]
    fn set_good_after_time_alias(&mut self, v: String) { self.good_after_time = v; }
    #[getter(goodTillDate)]
    fn get_good_till_date_alias(&self) -> String { self.good_till_date.clone() }
    #[setter(goodTillDate)]
    fn set_good_till_date_alias(&mut self, v: String) { self.good_till_date = v; }
    #[getter(hedgeParam)]
    fn get_hedge_param_alias(&self) -> String { self.hedge_param.clone() }
    #[setter(hedgeParam)]
    fn set_hedge_param_alias(&mut self, v: String) { self.hedge_param = v; }
    #[getter(hedgeType)]
    fn get_hedge_type_alias(&self) -> String { self.hedge_type.clone() }
    #[setter(hedgeType)]
    fn set_hedge_type_alias(&mut self, v: String) { self.hedge_type = v; }
    #[getter(ignoreOpenAuction)]
    fn get_ignore_open_auction_alias(&self) -> bool { self.ignore_open_auction }
    #[setter(ignoreOpenAuction)]
    fn set_ignore_open_auction_alias(&mut self, v: bool) { self.ignore_open_auction = v; }
    #[getter(imbalanceOnly)]
    fn get_imbalance_only_alias(&self) -> bool { self.imbalance_only }
    #[setter(imbalanceOnly)]
    fn set_imbalance_only_alias(&mut self, v: bool) { self.imbalance_only = v; }
    #[getter(includeOvernight)]
    fn get_include_overnight_alias(&self) -> bool { self.include_overnight }
    #[setter(includeOvernight)]
    fn set_include_overnight_alias(&mut self, v: bool) { self.include_overnight = v; }
    #[getter(isOmsContainer)]
    fn get_is_oms_container_alias(&self) -> bool { self.is_oms_container }
    #[setter(isOmsContainer)]
    fn set_is_oms_container_alias(&mut self, v: bool) { self.is_oms_container = v; }
    #[getter(isPeggedChangeAmountDecrease)]
    fn get_is_pegged_change_amount_decrease_alias(&self) -> bool { self.is_pegged_change_amount_decrease }
    #[setter(isPeggedChangeAmountDecrease)]
    fn set_is_pegged_change_amount_decrease_alias(&mut self, v: bool) { self.is_pegged_change_amount_decrease = v; }
    #[getter(lmtPriceOffset)]
    fn get_lmt_price_offset_alias(&self) -> f64 { self.lmt_price_offset }
    #[setter(lmtPriceOffset)]
    fn set_lmt_price_offset_alias(&mut self, v: f64) { self.lmt_price_offset = v; }
    #[getter(manualOrderIndicator)]
    fn get_manual_order_indicator_alias(&self) -> i32 { self.manual_order_indicator }
    #[setter(manualOrderIndicator)]
    fn set_manual_order_indicator_alias(&mut self, v: i32) { self.manual_order_indicator = v; }
    #[getter(manualOrderTime)]
    fn get_manual_order_time_alias(&self) -> String { self.manual_order_time.clone() }
    #[setter(manualOrderTime)]
    fn set_manual_order_time_alias(&mut self, v: String) { self.manual_order_time = v; }
    #[getter(midOffsetAtHalf)]
    fn get_mid_offset_at_half_alias(&self) -> f64 { self.mid_offset_at_half }
    #[setter(midOffsetAtHalf)]
    fn set_mid_offset_at_half_alias(&mut self, v: f64) { self.mid_offset_at_half = v; }
    #[getter(midOffsetAtWhole)]
    fn get_mid_offset_at_whole_alias(&self) -> f64 { self.mid_offset_at_whole }
    #[setter(midOffsetAtWhole)]
    fn set_mid_offset_at_whole_alias(&mut self, v: f64) { self.mid_offset_at_whole = v; }
    #[getter(mifid2DecisionAlgo)]
    fn get_mifid2_decision_algo_alias(&self) -> String { self.mifid2_decision_algo.clone() }
    #[setter(mifid2DecisionAlgo)]
    fn set_mifid2_decision_algo_alias(&mut self, v: String) { self.mifid2_decision_algo = v; }
    #[getter(mifid2DecisionMaker)]
    fn get_mifid2_decision_maker_alias(&self) -> String { self.mifid2_decision_maker.clone() }
    #[setter(mifid2DecisionMaker)]
    fn set_mifid2_decision_maker_alias(&mut self, v: String) { self.mifid2_decision_maker = v; }
    #[getter(mifid2ExecutionAlgo)]
    fn get_mifid2_execution_algo_alias(&self) -> String { self.mifid2_execution_algo.clone() }
    #[setter(mifid2ExecutionAlgo)]
    fn set_mifid2_execution_algo_alias(&mut self, v: String) { self.mifid2_execution_algo = v; }
    #[getter(mifid2ExecutionTrader)]
    fn get_mifid2_execution_trader_alias(&self) -> String { self.mifid2_execution_trader.clone() }
    #[setter(mifid2ExecutionTrader)]
    fn set_mifid2_execution_trader_alias(&mut self, v: String) { self.mifid2_execution_trader = v; }
    #[getter(minCompeteSize)]
    fn get_min_compete_size_alias(&self) -> i32 { self.min_compete_size }
    #[setter(minCompeteSize)]
    fn set_min_compete_size_alias(&mut self, v: i32) { self.min_compete_size = v; }
    #[getter(minQty)]
    fn get_min_qty_alias(&self) -> i32 { self.min_qty }
    #[setter(minQty)]
    fn set_min_qty_alias(&mut self, v: i32) { self.min_qty = v; }
    #[getter(minTradeQty)]
    fn get_min_trade_qty_alias(&self) -> i32 { self.min_trade_qty }
    #[setter(minTradeQty)]
    fn set_min_trade_qty_alias(&mut self, v: i32) { self.min_trade_qty = v; }
    #[getter(modelCode)]
    fn get_model_code_alias(&self) -> String { self.model_code.clone() }
    #[setter(modelCode)]
    fn set_model_code_alias(&mut self, v: String) { self.model_code = v; }
    #[getter(notHeld)]
    fn get_not_held_alias(&self) -> bool { self.not_held }
    #[setter(notHeld)]
    fn set_not_held_alias(&mut self, v: bool) { self.not_held = v; }
    #[getter(ocaGroup)]
    fn get_oca_group_alias(&self) -> String { self.oca_group.clone() }
    #[setter(ocaGroup)]
    fn set_oca_group_alias(&mut self, v: String) { self.oca_group = v; }
    #[getter(ocaType)]
    fn get_oca_type_alias(&self) -> i32 { self.oca_type }
    #[setter(ocaType)]
    fn set_oca_type_alias(&mut self, v: i32) { self.oca_type = v; }
    #[getter(openClose)]
    fn get_open_close_alias(&self) -> String { self.open_close.clone() }
    #[setter(openClose)]
    fn set_open_close_alias(&mut self, v: String) { self.open_close = v; }
    #[getter(optOutSmartRouting)]
    fn get_opt_out_smart_routing_alias(&self) -> bool { self.opt_out_smart_routing }
    #[setter(optOutSmartRouting)]
    fn set_opt_out_smart_routing_alias(&mut self, v: bool) { self.opt_out_smart_routing = v; }
    #[getter(orderComboLegs)]
    fn get_order_combo_legs_alias(&self, py: Python<'_>) -> Py<PyAny> { self.order_combo_legs.clone_ref(py) }
    #[setter(orderComboLegs)]
    fn set_order_combo_legs_alias(&mut self, v: Py<PyAny>) { self.order_combo_legs = v; }
    #[getter(orderMiscOptions)]
    fn get_order_misc_options_alias(&self, py: Python<'_>) -> Py<PyAny> { self.order_misc_options.clone_ref(py) }
    #[setter(orderMiscOptions)]
    fn set_order_misc_options_alias(&mut self, v: Py<PyAny>) { self.order_misc_options = v; }
    #[getter(orderRef)]
    fn get_order_ref_alias(&self) -> String { self.order_ref.clone() }
    #[setter(orderRef)]
    fn set_order_ref_alias(&mut self, v: String) { self.order_ref = v; }
    #[getter(outsideRth)]
    fn get_outside_rth_alias(&self) -> bool { self.outside_rth }
    #[setter(outsideRth)]
    fn set_outside_rth_alias(&mut self, v: bool) { self.outside_rth = v; }
    #[getter(overridePercentageConstraints)]
    fn get_override_percentage_constraints_alias(&self) -> bool { self.override_percentage_constraints }
    #[setter(overridePercentageConstraints)]
    fn set_override_percentage_constraints_alias(&mut self, v: bool) { self.override_percentage_constraints = v; }
    #[getter(parentId)]
    fn get_parent_id_alias(&self) -> i64 { self.parent_id }
    #[setter(parentId)]
    fn set_parent_id_alias(&mut self, v: i64) { self.parent_id = v; }
    #[getter(parentPermId)]
    fn get_parent_perm_id_alias(&self) -> i64 { self.parent_perm_id }
    #[setter(parentPermId)]
    fn set_parent_perm_id_alias(&mut self, v: i64) { self.parent_perm_id = v; }
    #[getter(peggedChangeAmount)]
    fn get_pegged_change_amount_alias(&self) -> f64 { self.pegged_change_amount }
    #[setter(peggedChangeAmount)]
    fn set_pegged_change_amount_alias(&mut self, v: f64) { self.pegged_change_amount = v; }
    #[getter(percentOffset)]
    fn get_percent_offset_alias(&self) -> f64 { self.percent_offset }
    #[setter(percentOffset)]
    fn set_percent_offset_alias(&mut self, v: f64) { self.percent_offset = v; }
    #[getter(permId)]
    fn get_perm_id_alias(&self) -> i64 { self.perm_id }
    #[setter(permId)]
    fn set_perm_id_alias(&mut self, v: i64) { self.perm_id = v; }
    #[getter(postOnly)]
    fn get_post_only_alias(&self) -> bool { self.post_only }
    #[setter(postOnly)]
    fn set_post_only_alias(&mut self, v: bool) { self.post_only = v; }
    #[getter(postToAts)]
    fn get_post_to_ats_alias(&self) -> i32 { self.post_to_ats }
    #[setter(postToAts)]
    fn set_post_to_ats_alias(&mut self, v: i32) { self.post_to_ats = v; }
    #[getter(professionalCustomer)]
    fn get_professional_customer_alias(&self) -> bool { self.professional_customer }
    #[setter(professionalCustomer)]
    fn set_professional_customer_alias(&mut self, v: bool) { self.professional_customer = v; }
    #[getter(ptOrderId)]
    fn get_pt_order_id_alias(&self) -> i32 { self.pt_order_id }
    #[setter(ptOrderId)]
    fn set_pt_order_id_alias(&mut self, v: i32) { self.pt_order_id = v; }
    #[getter(ptOrderType)]
    fn get_pt_order_type_alias(&self) -> String { self.pt_order_type.clone() }
    #[setter(ptOrderType)]
    fn set_pt_order_type_alias(&mut self, v: String) { self.pt_order_type = v; }
    #[getter(randomizePrice)]
    fn get_randomize_price_alias(&self) -> bool { self.randomize_price }
    #[setter(randomizePrice)]
    fn set_randomize_price_alias(&mut self, v: bool) { self.randomize_price = v; }
    #[getter(randomizeSize)]
    fn get_randomize_size_alias(&self) -> bool { self.randomize_size }
    #[setter(randomizeSize)]
    fn set_randomize_size_alias(&mut self, v: bool) { self.randomize_size = v; }
    #[getter(refFuturesConId)]
    fn get_ref_futures_con_id_alias(&self) -> i32 { self.ref_futures_con_id }
    #[setter(refFuturesConId)]
    fn set_ref_futures_con_id_alias(&mut self, v: i32) { self.ref_futures_con_id = v; }
    #[getter(referenceChangeAmount)]
    fn get_reference_change_amount_alias(&self) -> f64 { self.reference_change_amount }
    #[setter(referenceChangeAmount)]
    fn set_reference_change_amount_alias(&mut self, v: f64) { self.reference_change_amount = v; }
    #[getter(referenceContractId)]
    fn get_reference_contract_id_alias(&self) -> i32 { self.reference_contract_id }
    #[setter(referenceContractId)]
    fn set_reference_contract_id_alias(&mut self, v: i32) { self.reference_contract_id = v; }
    #[getter(referenceExchangeId)]
    fn get_reference_exchange_id_alias(&self) -> String { self.reference_exchange_id.clone() }
    #[setter(referenceExchangeId)]
    fn set_reference_exchange_id_alias(&mut self, v: String) { self.reference_exchange_id = v; }
    #[getter(referencePriceType)]
    fn get_reference_price_type_alias(&self) -> i32 { self.reference_price_type }
    #[setter(referencePriceType)]
    fn set_reference_price_type_alias(&mut self, v: i32) { self.reference_price_type = v; }
    #[getter(routeMarketableToBbo)]
    fn get_route_marketable_to_bbo_alias(&self) -> Option<bool> { self.route_marketable_to_bbo }
    #[setter(routeMarketableToBbo)]
    fn set_route_marketable_to_bbo_alias(&mut self, v: Option<bool>) { self.route_marketable_to_bbo = v; }
    #[getter(rule80A)]
    fn get_rule80a_alias(&self) -> String { self.rule80a.clone() }
    #[setter(rule80A)]
    fn set_rule80a_alias(&mut self, v: String) { self.rule80a = v; }
    #[getter(scaleAutoReset)]
    fn get_scale_auto_reset_alias(&self) -> bool { self.scale_auto_reset }
    #[setter(scaleAutoReset)]
    fn set_scale_auto_reset_alias(&mut self, v: bool) { self.scale_auto_reset = v; }
    #[getter(scaleInitFillQty)]
    fn get_scale_init_fill_qty_alias(&self) -> i32 { self.scale_init_fill_qty }
    #[setter(scaleInitFillQty)]
    fn set_scale_init_fill_qty_alias(&mut self, v: i32) { self.scale_init_fill_qty = v; }
    #[getter(scaleInitLevelSize)]
    fn get_scale_init_level_size_alias(&self) -> i32 { self.scale_init_level_size }
    #[setter(scaleInitLevelSize)]
    fn set_scale_init_level_size_alias(&mut self, v: i32) { self.scale_init_level_size = v; }
    #[getter(scaleInitPosition)]
    fn get_scale_init_position_alias(&self) -> i32 { self.scale_init_position }
    #[setter(scaleInitPosition)]
    fn set_scale_init_position_alias(&mut self, v: i32) { self.scale_init_position = v; }
    #[getter(scalePriceAdjustInterval)]
    fn get_scale_price_adjust_interval_alias(&self) -> i32 { self.scale_price_adjust_interval }
    #[setter(scalePriceAdjustInterval)]
    fn set_scale_price_adjust_interval_alias(&mut self, v: i32) { self.scale_price_adjust_interval = v; }
    #[getter(scalePriceAdjustValue)]
    fn get_scale_price_adjust_value_alias(&self) -> f64 { self.scale_price_adjust_value }
    #[setter(scalePriceAdjustValue)]
    fn set_scale_price_adjust_value_alias(&mut self, v: f64) { self.scale_price_adjust_value = v; }
    #[getter(scalePriceIncrement)]
    fn get_scale_price_increment_alias(&self) -> f64 { self.scale_price_increment }
    #[setter(scalePriceIncrement)]
    fn set_scale_price_increment_alias(&mut self, v: f64) { self.scale_price_increment = v; }
    #[getter(scaleProfitOffset)]
    fn get_scale_profit_offset_alias(&self) -> f64 { self.scale_profit_offset }
    #[setter(scaleProfitOffset)]
    fn set_scale_profit_offset_alias(&mut self, v: f64) { self.scale_profit_offset = v; }
    #[getter(scaleRandomPercent)]
    fn get_scale_random_percent_alias(&self) -> bool { self.scale_random_percent }
    #[setter(scaleRandomPercent)]
    fn set_scale_random_percent_alias(&mut self, v: bool) { self.scale_random_percent = v; }
    #[getter(scaleSubsLevelSize)]
    fn get_scale_subs_level_size_alias(&self) -> i32 { self.scale_subs_level_size }
    #[setter(scaleSubsLevelSize)]
    fn set_scale_subs_level_size_alias(&mut self, v: i32) { self.scale_subs_level_size = v; }
    #[getter(scaleTable)]
    fn get_scale_table_alias(&self) -> String { self.scale_table.clone() }
    #[setter(scaleTable)]
    fn set_scale_table_alias(&mut self, v: String) { self.scale_table = v; }
    #[getter(seekPriceImprovement)]
    fn get_seek_price_improvement_alias(&self) -> Option<bool> { self.seek_price_improvement }
    #[setter(seekPriceImprovement)]
    fn set_seek_price_improvement_alias(&mut self, v: Option<bool>) { self.seek_price_improvement = v; }
    #[getter(settlingFirm)]
    fn get_settling_firm_alias(&self) -> String { self.settling_firm.clone() }
    #[setter(settlingFirm)]
    fn set_settling_firm_alias(&mut self, v: String) { self.settling_firm = v; }
    #[getter(shortSaleSlot)]
    fn get_short_sale_slot_alias(&self) -> i32 { self.short_sale_slot }
    #[setter(shortSaleSlot)]
    fn set_short_sale_slot_alias(&mut self, v: i32) { self.short_sale_slot = v; }
    #[getter(slOrderId)]
    fn get_sl_order_id_alias(&self) -> i32 { self.sl_order_id }
    #[setter(slOrderId)]
    fn set_sl_order_id_alias(&mut self, v: i32) { self.sl_order_id = v; }
    #[getter(slOrderType)]
    fn get_sl_order_type_alias(&self) -> String { self.sl_order_type.clone() }
    #[setter(slOrderType)]
    fn set_sl_order_type_alias(&mut self, v: String) { self.sl_order_type = v; }
    #[getter(smartComboRoutingParams)]
    fn get_smart_combo_routing_params_alias(&self, py: Python<'_>) -> Py<PyAny> { self.smart_combo_routing_params.clone_ref(py) }
    #[setter(smartComboRoutingParams)]
    fn set_smart_combo_routing_params_alias(&mut self, v: Py<PyAny>) { self.smart_combo_routing_params = v; }
    #[getter(softDollarTier)]
    fn get_soft_dollar_tier(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let tier = SoftDollarTierPy {
            name: self.soft_dollar_tier_name.clone(),
            val: self.soft_dollar_tier_val.clone(),
            display_name: self.soft_dollar_tier_display_name.clone(),
        };
        Ok(Py::new(py, tier)?.into_any().into())
    }
    #[getter(startingPrice)]
    fn get_starting_price_alias(&self) -> f64 { self.starting_price }
    #[setter(startingPrice)]
    fn set_starting_price_alias(&mut self, v: f64) { self.starting_price = v; }
    #[getter(stockRangeLower)]
    fn get_stock_range_lower_alias(&self) -> f64 { self.stock_range_lower }
    #[setter(stockRangeLower)]
    fn set_stock_range_lower_alias(&mut self, v: f64) { self.stock_range_lower = v; }
    #[getter(stockRangeUpper)]
    fn get_stock_range_upper_alias(&self) -> f64 { self.stock_range_upper }
    #[setter(stockRangeUpper)]
    fn set_stock_range_upper_alias(&mut self, v: f64) { self.stock_range_upper = v; }
    #[getter(stockRefPrice)]
    fn get_stock_ref_price_alias(&self) -> f64 { self.stock_ref_price }
    #[setter(stockRefPrice)]
    fn set_stock_ref_price_alias(&mut self, v: f64) { self.stock_ref_price = v; }
    #[getter(sweepToFill)]
    fn get_sweep_to_fill_alias(&self) -> bool { self.sweep_to_fill }
    #[setter(sweepToFill)]
    fn set_sweep_to_fill_alias(&mut self, v: bool) { self.sweep_to_fill = v; }
    #[getter(trailStopPrice)]
    fn get_trail_stop_price_alias(&self) -> f64 { self.trail_stop_price }
    #[setter(trailStopPrice)]
    fn set_trail_stop_price_alias(&mut self, v: f64) { self.trail_stop_price = v; }
    #[getter(trailingPercent)]
    fn get_trailing_percent_alias(&self) -> f64 { self.trailing_percent }
    #[setter(trailingPercent)]
    fn set_trailing_percent_alias(&mut self, v: f64) { self.trailing_percent = v; }
    #[getter(triggerMethod)]
    fn get_trigger_method_alias(&self) -> i32 { self.trigger_method }
    #[setter(triggerMethod)]
    fn set_trigger_method_alias(&mut self, v: i32) { self.trigger_method = v; }
    #[getter(triggerPrice)]
    fn get_trigger_price_alias(&self) -> f64 { self.trigger_price }
    #[setter(triggerPrice)]
    fn set_trigger_price_alias(&mut self, v: f64) { self.trigger_price = v; }
    #[getter(usePriceMgmtAlgo)]
    fn get_use_price_mgmt_algo_alias(&self) -> Option<bool> { self.use_price_mgmt_algo }
    #[setter(usePriceMgmtAlgo)]
    fn set_use_price_mgmt_algo_alias(&mut self, v: Option<bool>) { self.use_price_mgmt_algo = v; }
    #[getter(volatilityType)]
    fn get_volatility_type_alias(&self) -> i32 { self.volatility_type }
    #[setter(volatilityType)]
    fn set_volatility_type_alias(&mut self, v: i32) { self.volatility_type = v; }
    #[getter(whatIf)]
    fn get_what_if_alias(&self) -> bool { self.what_if }
    #[setter(whatIf)]
    fn set_what_if_alias(&mut self, v: bool) { self.what_if = v; }
    #[getter(whatIfType)]
    fn get_what_if_type_alias(&self) -> i32 { self.what_if_type }
    #[setter(whatIfType)]
    fn set_what_if_type_alias(&mut self, v: i32) { self.what_if_type = v; }
}

/// Delegate conversion helpers to the Rust API types.
impl Order {
    /// Parse the action string to Side. Delegates to `api::Order::side()`.
    pub fn side(&self) -> PyResult<Side> {
        self.to_api().side()
            .map_err(|e| PyRuntimeError::new_err(e))
    }

    /// Parse the TIF string to FIX byte. Delegates to `api::Order::tif_byte()`.
    pub fn tif_byte(&self) -> u8 {
        self.to_api().tif_byte()
    }

    /// Build OrderAttrs from Order fields. Delegates to `api::Order::attrs()`.
    pub fn attrs(&self) -> OrderAttrs {
        self.to_api().attrs()
    }

    /// Check if the order has any extended attributes set. Delegates to `api::Order::has_extended_attrs()`.
    pub fn has_extended_attrs(&self) -> bool {
        self.to_api().has_extended_attrs()
    }
}

// ── Conversions between Python compat types and Rust API types ──

impl Contract {
    /// Convert to Rust API Contract. The official API's unset strike
    /// (`f64::MAX`) is the Rust API's unset strike, 0.
    pub fn to_api(&self) -> crate::api::types::Contract {
        crate::api::types::Contract {
            con_id: self.con_id,
            symbol: self.symbol.clone(),
            sec_type: self.sec_type.clone(),
            exchange: self.exchange.clone(),
            currency: self.currency.clone(),
            last_trade_date_or_contract_month: self.last_trade_date_or_contract_month.clone(),
            strike: if self.strike == f64::MAX { 0.0 } else { self.strike },
            right: self.right.clone(),
            multiplier: self.multiplier.clone(),
            local_symbol: self.local_symbol.clone(),
            primary_exchange: self.primary_exchange.clone(),
            trading_class: self.trading_class.clone(),
            last_trade_date: self.last_trade_date.clone(),
            include_expired: self.include_expired,
            sec_id_type: self.sec_id_type.clone(),
            sec_id: self.sec_id.clone(),
            description: self.description.clone(),
            issuer_id: self.issuer_id.clone(),
            combo_legs_descrip: self.combo_legs_descrip.clone(),
            ..Default::default()
        }
    }
}

impl Order {
    /// Convert the conditions (this module's condition classes) to the
    /// engine's.
    pub fn convert_conditions(&self, py: Python<'_>) -> Vec<OrderCondition> {
        items(py, &self.conditions).iter().filter_map(|any| {
            if let Ok(c) = any.cast::<PriceCondition>() { return Some(c.borrow().to_internal()); }
            if let Ok(c) = any.cast::<TimeCondition>() { return Some(c.borrow().to_internal()); }
            if let Ok(c) = any.cast::<MarginCondition>() { return Some(c.borrow().to_internal()); }
            if let Ok(c) = any.cast::<ExecutionCondition>() { return Some(c.borrow().to_internal()); }
            if let Ok(c) = any.cast::<VolumeCondition>() { return Some(c.borrow().to_internal()); }
            if let Ok(c) = any.cast::<PercentChangeCondition>() { return Some(c.borrow().to_internal()); }
            log::warn!("Unknown order condition type, skipping");
            None
        }).collect()
    }

    /// TagValue items (this module's or the official API's) of a list
    /// attribute.
    fn tag_values(py: Python<'_>, list: &Py<PyAny>) -> Vec<crate::api::types::TagValue> {
        items(py, list).iter().map(|tv| crate::api::types::TagValue {
            tag: tv.getattr("tag").and_then(|v| v.str().map(|s| s.to_string())).unwrap_or_default(),
            value: tv.getattr("value").and_then(|v| v.str().map(|s| s.to_string())).unwrap_or_default(),
        }).collect()
    }

    /// Convert to Rust API Order: every field; the conditions and the
    /// per-leg prices need the interpreter (`convert_conditions`,
    /// `convert_order_combo_legs`).
    pub fn to_api(&self) -> crate::api::types::Order {
        let (algo_params, smart_combo_routing_params, order_misc_options) = Python::attach(|py| (
            Self::tag_values(py, &self.algo_params),
            Self::tag_values(py, &self.smart_combo_routing_params),
            Self::tag_values(py, &self.order_misc_options),
        ));
        crate::api::types::Order {
            order_id: self.order_id,
            action: self.action.clone(),
            total_quantity: self.total_quantity,
            order_type: self.order_type.clone(),
            lmt_price: self.lmt_price,
            aux_price: self.aux_price,
            tif: self.tif.clone(),
            outside_rth: self.outside_rth,
            display_size: self.display_size,
            min_qty: self.min_qty,
            hidden: self.hidden,
            good_after_time: self.good_after_time.clone(),
            good_till_date: self.good_till_date.clone(),
            oca_group: self.oca_group.clone(),
            trailing_percent: self.trailing_percent,
            algo_strategy: self.algo_strategy.clone(),
            what_if: self.what_if,
            cash_qty: self.cash_qty,
            parent_id: self.parent_id,
            transmit: self.transmit,
            discretionary_amt: self.discretionary_amt,
            sweep_to_fill: self.sweep_to_fill,
            all_or_none: self.all_or_none,
            trigger_method: self.trigger_method,
            adjusted_order_type: self.adjusted_order_type.clone(),
            trigger_price: self.trigger_price,
            adjusted_stop_price: self.adjusted_stop_price,
            adjusted_stop_limit_price: self.adjusted_stop_limit_price,
            conditions_ignore_rth: self.conditions_ignore_rth,
            conditions_cancel_order: self.conditions_cancel_order,
            account: self.account.clone(),
            active_start_time: self.active_start_time.clone(),
            active_stop_time: self.active_stop_time.clone(),
            adjustable_trailing_unit: self.adjustable_trailing_unit,
            adjusted_trailing_amount: self.adjusted_trailing_amount,
            advanced_error_override: self.advanced_error_override.clone(),
            algo_id: self.algo_id.clone(),
            allow_pre_open: self.allow_pre_open,
            auction_strategy: self.auction_strategy,
            auto_cancel_date: self.auto_cancel_date.clone(),
            auto_cancel_parent: self.auto_cancel_parent,
            basis_points: self.basis_points,
            basis_points_type: self.basis_points_type,
            block_order: self.block_order,
            bond_accrued_interest: self.bond_accrued_interest.clone(),
            clearing_account: self.clearing_account.clone(),
            clearing_intent: self.clearing_intent.clone(),
            client_id: self.client_id,
            compete_against_best_offset: self.compete_against_best_offset,
            continuous_update: self.continuous_update,
            customer_account: self.customer_account.clone(),
            deactivate: self.deactivate,
            delta: self.delta,
            delta_neutral_aux_price: self.delta_neutral_aux_price,
            delta_neutral_clearing_account: self.delta_neutral_clearing_account.clone(),
            delta_neutral_clearing_intent: self.delta_neutral_clearing_intent.clone(),
            delta_neutral_con_id: self.delta_neutral_con_id,
            delta_neutral_designated_location: self.delta_neutral_designated_location.clone(),
            delta_neutral_open_close: self.delta_neutral_open_close.clone(),
            delta_neutral_order_type: self.delta_neutral_order_type.clone(),
            delta_neutral_settling_firm: self.delta_neutral_settling_firm.clone(),
            delta_neutral_short_sale: self.delta_neutral_short_sale,
            delta_neutral_short_sale_slot: self.delta_neutral_short_sale_slot,
            designated_location: self.designated_location.clone(),
            discretionary_up_to_limit_price: self.discretionary_up_to_limit_price,
            dont_use_auto_price_for_hedge: self.dont_use_auto_price_for_hedge,
            duration: self.duration,
            exempt_code: self.exempt_code,
            ext_operator: self.ext_operator.clone(),
            fa_group: self.fa_group.clone(),
            fa_method: self.fa_method.clone(),
            fa_percentage: self.fa_percentage.clone(),
            filled_quantity: self.filled_quantity,
            hedge_param: self.hedge_param.clone(),
            hedge_type: self.hedge_type.clone(),
            ignore_open_auction: self.ignore_open_auction,
            imbalance_only: self.imbalance_only,
            include_overnight: self.include_overnight,
            is_oms_container: self.is_oms_container,
            is_pegged_change_amount_decrease: self.is_pegged_change_amount_decrease,
            lmt_price_offset: self.lmt_price_offset,
            manual_order_indicator: self.manual_order_indicator,
            manual_order_time: self.manual_order_time.clone(),
            mid_offset_at_half: self.mid_offset_at_half,
            mid_offset_at_whole: self.mid_offset_at_whole,
            mifid2_decision_algo: self.mifid2_decision_algo.clone(),
            mifid2_decision_maker: self.mifid2_decision_maker.clone(),
            mifid2_execution_algo: self.mifid2_execution_algo.clone(),
            mifid2_execution_trader: self.mifid2_execution_trader.clone(),
            min_compete_size: self.min_compete_size,
            min_trade_qty: self.min_trade_qty,
            model_code: self.model_code.clone(),
            not_held: self.not_held,
            oca_type: self.oca_type,
            open_close: self.open_close.clone(),
            opt_out_smart_routing: self.opt_out_smart_routing,
            order_ref: self.order_ref.clone(),
            origin: self.origin,
            override_percentage_constraints: self.override_percentage_constraints,
            parent_perm_id: self.parent_perm_id,
            pegged_change_amount: self.pegged_change_amount,
            percent_offset: self.percent_offset,
            perm_id: self.perm_id,
            post_only: self.post_only,
            post_to_ats: self.post_to_ats,
            professional_customer: self.professional_customer,
            pt_order_id: self.pt_order_id,
            pt_order_type: self.pt_order_type.clone(),
            randomize_price: self.randomize_price,
            randomize_size: self.randomize_size,
            ref_futures_con_id: self.ref_futures_con_id,
            reference_change_amount: self.reference_change_amount,
            reference_contract_id: self.reference_contract_id,
            reference_exchange_id: self.reference_exchange_id.clone(),
            reference_price_type: self.reference_price_type,
            rule80a: self.rule80a.clone(),
            scale_auto_reset: self.scale_auto_reset,
            scale_init_fill_qty: self.scale_init_fill_qty,
            scale_init_level_size: self.scale_init_level_size,
            scale_init_position: self.scale_init_position,
            scale_price_adjust_interval: self.scale_price_adjust_interval,
            scale_price_adjust_value: self.scale_price_adjust_value,
            scale_price_increment: self.scale_price_increment,
            scale_profit_offset: self.scale_profit_offset,
            scale_random_percent: self.scale_random_percent,
            scale_subs_level_size: self.scale_subs_level_size,
            scale_table: self.scale_table.clone(),
            settling_firm: self.settling_firm.clone(),
            shareholder: self.shareholder.clone(),
            short_sale_slot: self.short_sale_slot,
            sl_order_id: self.sl_order_id,
            sl_order_type: self.sl_order_type.clone(),
            soft_dollar_tier_name: self.soft_dollar_tier_name.clone(),
            soft_dollar_tier_val: self.soft_dollar_tier_val.clone(),
            soft_dollar_tier_display_name: self.soft_dollar_tier_display_name.clone(),
            solicited: self.solicited,
            starting_price: self.starting_price,
            stock_range_lower: self.stock_range_lower,
            stock_range_upper: self.stock_range_upper,
            stock_ref_price: self.stock_ref_price,
            submitter: self.submitter.clone(),
            trail_stop_price: self.trail_stop_price,
            volatility: self.volatility,
            volatility_type: self.volatility_type,
            what_if_type: self.what_if_type,
            algo_params,
            smart_combo_routing_params,
            order_misc_options,
            // None is the API's unset value (ibx#492).
            use_price_mgmt_algo: self.use_price_mgmt_algo.map_or(i32::MAX, i32::from),
            route_marketable_to_bbo: self.route_marketable_to_bbo.unwrap_or(false),
            seek_price_improvement: self.seek_price_improvement.unwrap_or(false),
            ..Default::default()
        }
    }

    /// The order an order callback shows, from the Rust API order: every
    /// field; the empty lists are None, as the official API's decoding
    /// leaves them. The conditions are not shown.
    pub fn from_api(py: Python<'_>, o: &crate::api::types::Order) -> PyResult<Self> {
        let tag_values = |list: &[crate::api::types::TagValue]| -> PyResult<Py<PyAny>> {
            let objs = list.iter()
                .map(|tv| Py::new(py, TagValue { tag: tv.tag.clone(), value: tv.value.clone() }))
                .collect::<PyResult<Vec<_>>>()?;
            list_or_none(py, objs)
        };
        let legs = o.order_combo_legs.iter()
            .map(|price| Py::new(py, OrderComboLeg { price: *price }))
            .collect::<PyResult<Vec<_>>>()?;
        Ok(Self {
            order_id: o.order_id,
            action: o.action.clone(),
            total_quantity: o.total_quantity,
            order_type: o.order_type.clone(),
            lmt_price: o.lmt_price,
            aux_price: o.aux_price,
            tif: o.tif.clone(),
            outside_rth: o.outside_rth,
            display_size: o.display_size,
            min_qty: o.min_qty,
            hidden: o.hidden,
            good_after_time: o.good_after_time.clone(),
            good_till_date: o.good_till_date.clone(),
            oca_group: o.oca_group.clone(),
            trailing_percent: o.trailing_percent,
            algo_strategy: o.algo_strategy.clone(),
            what_if: o.what_if,
            cash_qty: o.cash_qty,
            parent_id: o.parent_id,
            transmit: o.transmit,
            discretionary_amt: o.discretionary_amt,
            sweep_to_fill: o.sweep_to_fill,
            all_or_none: o.all_or_none,
            trigger_method: o.trigger_method,
            adjusted_order_type: o.adjusted_order_type.clone(),
            trigger_price: o.trigger_price,
            adjusted_stop_price: o.adjusted_stop_price,
            adjusted_stop_limit_price: o.adjusted_stop_limit_price,
            conditions_ignore_rth: o.conditions_ignore_rth,
            conditions_cancel_order: o.conditions_cancel_order,
            account: o.account.clone(),
            active_start_time: o.active_start_time.clone(),
            active_stop_time: o.active_stop_time.clone(),
            adjustable_trailing_unit: o.adjustable_trailing_unit,
            adjusted_trailing_amount: o.adjusted_trailing_amount,
            advanced_error_override: o.advanced_error_override.clone(),
            algo_id: o.algo_id.clone(),
            allow_pre_open: o.allow_pre_open,
            auction_strategy: o.auction_strategy,
            auto_cancel_date: o.auto_cancel_date.clone(),
            auto_cancel_parent: o.auto_cancel_parent,
            basis_points: o.basis_points,
            basis_points_type: o.basis_points_type,
            block_order: o.block_order,
            bond_accrued_interest: o.bond_accrued_interest.clone(),
            clearing_account: o.clearing_account.clone(),
            clearing_intent: o.clearing_intent.clone(),
            client_id: o.client_id,
            compete_against_best_offset: o.compete_against_best_offset,
            continuous_update: o.continuous_update,
            customer_account: o.customer_account.clone(),
            deactivate: o.deactivate,
            delta: o.delta,
            delta_neutral_aux_price: o.delta_neutral_aux_price,
            delta_neutral_clearing_account: o.delta_neutral_clearing_account.clone(),
            delta_neutral_clearing_intent: o.delta_neutral_clearing_intent.clone(),
            delta_neutral_con_id: o.delta_neutral_con_id,
            delta_neutral_designated_location: o.delta_neutral_designated_location.clone(),
            delta_neutral_open_close: o.delta_neutral_open_close.clone(),
            delta_neutral_order_type: o.delta_neutral_order_type.clone(),
            delta_neutral_settling_firm: o.delta_neutral_settling_firm.clone(),
            delta_neutral_short_sale: o.delta_neutral_short_sale,
            delta_neutral_short_sale_slot: o.delta_neutral_short_sale_slot,
            designated_location: o.designated_location.clone(),
            discretionary_up_to_limit_price: o.discretionary_up_to_limit_price,
            dont_use_auto_price_for_hedge: o.dont_use_auto_price_for_hedge,
            duration: o.duration,
            exempt_code: o.exempt_code,
            ext_operator: o.ext_operator.clone(),
            fa_group: o.fa_group.clone(),
            fa_method: o.fa_method.clone(),
            fa_percentage: o.fa_percentage.clone(),
            filled_quantity: o.filled_quantity,
            hedge_param: o.hedge_param.clone(),
            hedge_type: o.hedge_type.clone(),
            ignore_open_auction: o.ignore_open_auction,
            imbalance_only: o.imbalance_only,
            include_overnight: o.include_overnight,
            is_oms_container: o.is_oms_container,
            is_pegged_change_amount_decrease: o.is_pegged_change_amount_decrease,
            lmt_price_offset: o.lmt_price_offset,
            manual_order_indicator: o.manual_order_indicator,
            manual_order_time: o.manual_order_time.clone(),
            mid_offset_at_half: o.mid_offset_at_half,
            mid_offset_at_whole: o.mid_offset_at_whole,
            mifid2_decision_algo: o.mifid2_decision_algo.clone(),
            mifid2_decision_maker: o.mifid2_decision_maker.clone(),
            mifid2_execution_algo: o.mifid2_execution_algo.clone(),
            mifid2_execution_trader: o.mifid2_execution_trader.clone(),
            min_compete_size: o.min_compete_size,
            min_trade_qty: o.min_trade_qty,
            model_code: o.model_code.clone(),
            not_held: o.not_held,
            oca_type: o.oca_type,
            open_close: o.open_close.clone(),
            opt_out_smart_routing: o.opt_out_smart_routing,
            order_ref: o.order_ref.clone(),
            origin: o.origin,
            override_percentage_constraints: o.override_percentage_constraints,
            parent_perm_id: o.parent_perm_id,
            pegged_change_amount: o.pegged_change_amount,
            percent_offset: o.percent_offset,
            perm_id: o.perm_id,
            post_only: o.post_only,
            post_to_ats: o.post_to_ats,
            professional_customer: o.professional_customer,
            pt_order_id: o.pt_order_id,
            pt_order_type: o.pt_order_type.clone(),
            randomize_price: o.randomize_price,
            randomize_size: o.randomize_size,
            ref_futures_con_id: o.ref_futures_con_id,
            reference_change_amount: o.reference_change_amount,
            reference_contract_id: o.reference_contract_id,
            reference_exchange_id: o.reference_exchange_id.clone(),
            reference_price_type: o.reference_price_type,
            rule80a: o.rule80a.clone(),
            scale_auto_reset: o.scale_auto_reset,
            scale_init_fill_qty: o.scale_init_fill_qty,
            scale_init_level_size: o.scale_init_level_size,
            scale_init_position: o.scale_init_position,
            scale_price_adjust_interval: o.scale_price_adjust_interval,
            scale_price_adjust_value: o.scale_price_adjust_value,
            scale_price_increment: o.scale_price_increment,
            scale_profit_offset: o.scale_profit_offset,
            scale_random_percent: o.scale_random_percent,
            scale_subs_level_size: o.scale_subs_level_size,
            scale_table: o.scale_table.clone(),
            settling_firm: o.settling_firm.clone(),
            shareholder: o.shareholder.clone(),
            short_sale_slot: o.short_sale_slot,
            sl_order_id: o.sl_order_id,
            sl_order_type: o.sl_order_type.clone(),
            soft_dollar_tier_name: o.soft_dollar_tier_name.clone(),
            soft_dollar_tier_val: o.soft_dollar_tier_val.clone(),
            soft_dollar_tier_display_name: o.soft_dollar_tier_display_name.clone(),
            solicited: o.solicited,
            starting_price: o.starting_price,
            stock_range_lower: o.stock_range_lower,
            stock_range_upper: o.stock_range_upper,
            stock_ref_price: o.stock_ref_price,
            submitter: o.submitter.clone(),
            trail_stop_price: o.trail_stop_price,
            volatility: o.volatility,
            volatility_type: o.volatility_type,
            what_if_type: o.what_if_type,
            algo_params: tag_values(&o.algo_params)?,
            smart_combo_routing_params: tag_values(&o.smart_combo_routing_params)?,
            order_misc_options: tag_values(&o.order_misc_options)?,
            order_combo_legs: list_or_none(py, legs)?,
            conditions: empty_list(),
            use_price_mgmt_algo: (o.use_price_mgmt_algo != i32::MAX).then_some(o.use_price_mgmt_algo != 0),
            route_marketable_to_bbo: o.route_marketable_to_bbo.then_some(true),
            seek_price_improvement: o.seek_price_improvement.then_some(true),
            hedge_max_size: i32::MAX,
        })
    }

    /// The per-leg prices of a combo order (ibx#470): the `price` of each
    /// orderComboLegs entry (this module's OrderComboLeg or the official
    /// API's), f64::MAX when it has none.
    pub fn convert_order_combo_legs(&self, py: Python<'_>) -> Vec<f64> {
        items(py, &self.order_combo_legs).iter().map(|obj| {
            obj.getattr("price").ok().and_then(|v| v.extract::<f64>().ok()).unwrap_or(f64::MAX)
        }).collect()
    }
}

impl Contract {
    /// The legs of a combo contract (ibx#470), read from this module's
    /// ComboLeg or the official API's: the camelCase attribute, else the
    /// snake_case one.
    pub fn convert_combo_legs(&self, py: Python<'_>) -> Vec<crate::api::types::ComboLeg> {
        items(py, &self.combo_legs).iter().map(|leg| {
            crate::api::types::ComboLeg {
                con_id: attr(leg, "conId", "con_id").unwrap_or(0),
                ratio: attr(leg, "ratio", "ratio").unwrap_or(0),
                action: attr(leg, "action", "action").unwrap_or_default(),
                exchange: attr(leg, "exchange", "exchange").unwrap_or_default(),
                open_close: attr(leg, "openClose", "open_close").unwrap_or(0),
                short_sale_slot: attr(leg, "shortSaleSlot", "short_sale_slot").unwrap_or(0),
                designated_location: attr(leg, "designatedLocation", "designated_location").unwrap_or_default(),
                exempt_code: attr(leg, "exemptCode", "exempt_code").unwrap_or(-1),
            }
        }).collect()
    }

    /// The contract a callback shows, from the Rust API contract: the
    /// fields the reference reports, its combo legs as this module's
    /// ComboLeg (ibx#470), the strike as reported (0 when none).
    pub fn from_api(py: Python<'_>, c: &crate::api::types::Contract) -> PyResult<Self> {
        let legs = c.combo_legs.iter()
            .map(|leg| Py::new(py, ComboLeg::from_api(leg)))
            .collect::<PyResult<Vec<_>>>()?;
        Ok(Contract {
            con_id: c.con_id,
            symbol: c.symbol.clone(),
            sec_type: c.sec_type.clone(),
            exchange: c.exchange.clone(),
            primary_exchange: c.primary_exchange.clone(),
            currency: c.currency.clone(),
            local_symbol: c.local_symbol.clone(),
            trading_class: c.trading_class.clone(),
            last_trade_date_or_contract_month: c.last_trade_date_or_contract_month.clone(),
            last_trade_date: c.last_trade_date.clone(),
            strike: c.strike,
            right: c.right.clone(),
            multiplier: c.multiplier.clone(),
            combo_legs_descrip: c.combo_legs_descrip.clone(),
            combo_legs: pyo3::types::PyList::new(py, legs)?.into_any().unbind(),
            ..Default::default()
        })
    }
}

// ── ComboLeg, OrderComboLeg ──

/// ibapi-compatible ComboLeg: one leg of a combo (BAG) contract (ibx#470).
#[pyclass(from_py_object)]
#[derive(Clone, Debug)]
pub struct ComboLeg {
    #[pyo3(get, set)]
    pub con_id: i64,
    #[pyo3(get, set)]
    pub ratio: i32,
    #[pyo3(get, set)]
    pub action: String,
    #[pyo3(get, set)]
    pub exchange: String,
    /// 0 same as the order, 1 open, 2 close, 3 unknown.
    #[pyo3(get, set)]
    pub open_close: i32,
    #[pyo3(get, set)]
    pub short_sale_slot: i32,
    #[pyo3(get, set)]
    pub designated_location: String,
    #[pyo3(get, set)]
    pub exempt_code: i32,
}

impl ComboLeg {
    fn from_api(leg: &crate::api::types::ComboLeg) -> Self {
        Self {
            con_id: leg.con_id, ratio: leg.ratio, action: leg.action.clone(), exchange: leg.exchange.clone(),
            open_close: leg.open_close, short_sale_slot: leg.short_sale_slot,
            designated_location: leg.designated_location.clone(), exempt_code: leg.exempt_code,
        }
    }
}

#[pymethods]
impl ComboLeg {
    #[new]
    #[pyo3(signature = (con_id=0, ratio=0, action="".to_string(), exchange="".to_string(), open_close=0, short_sale_slot=0, designated_location="".to_string(), exempt_code=-1))]
    #[allow(clippy::too_many_arguments)]
    fn new(con_id: i64, ratio: i32, action: String, exchange: String, open_close: i32,
        short_sale_slot: i32, designated_location: String, exempt_code: i32) -> Self {
        Self { con_id, ratio, action, exchange, open_close, short_sale_slot, designated_location, exempt_code }
    }

    fn __repr__(&self) -> String {
        format!("ComboLeg(conId={}, ratio={}, action='{}', exchange='{}')", self.con_id, self.ratio, self.action, self.exchange)
    }

    #[getter(conId)]
    fn get_con_id_alias(&self) -> i64 { self.con_id }
    #[setter(conId)]
    fn set_con_id_alias(&mut self, v: i64) { self.con_id = v; }
    #[getter(openClose)]
    fn get_open_close_alias(&self) -> i32 { self.open_close }
    #[setter(openClose)]
    fn set_open_close_alias(&mut self, v: i32) { self.open_close = v; }
    #[getter(shortSaleSlot)]
    fn get_short_sale_slot_alias(&self) -> i32 { self.short_sale_slot }
    #[setter(shortSaleSlot)]
    fn set_short_sale_slot_alias(&mut self, v: i32) { self.short_sale_slot = v; }
    #[getter(designatedLocation)]
    fn get_designated_location_alias(&self) -> String { self.designated_location.clone() }
    #[setter(designatedLocation)]
    fn set_designated_location_alias(&mut self, v: String) { self.designated_location = v; }
    #[getter(exemptCode)]
    fn get_exempt_code_alias(&self) -> i32 { self.exempt_code }
    #[setter(exemptCode)]
    fn set_exempt_code_alias(&mut self, v: i32) { self.exempt_code = v; }
}

/// ibapi-compatible OrderComboLeg: the price of one leg of a combo order
/// (ibx#470). Unset is f64::MAX, as the official API's.
#[pyclass(from_py_object)]
#[derive(Clone, Debug)]
pub struct OrderComboLeg {
    #[pyo3(get, set)]
    pub price: f64,
}

#[pymethods]
impl OrderComboLeg {
    #[new]
    #[pyo3(signature = (price=f64::MAX))]
    fn new(price: f64) -> Self {
        Self { price }
    }

    fn __repr__(&self) -> String {
        format!("OrderComboLeg(price={})", self.price)
    }
}

// ── TagValue ──

/// ibapi-compatible TagValue for algo parameters. As the official API's,
/// the constructor keeps the text of what it is given: `TagValue()` has
/// the tag and value 'None'.
#[pyclass(from_py_object)]
#[derive(Clone, Debug)]
pub struct TagValue {
    #[pyo3(get, set)]
    pub tag: String,
    #[pyo3(get, set)]
    pub value: String,
}

#[pymethods]
impl TagValue {
    #[new]
    #[pyo3(signature = (tag=None, value=None))]
    fn new(tag: Option<&Bound<'_, PyAny>>, value: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let text = |v: Option<&Bound<'_, PyAny>>| -> PyResult<String> {
            v.map_or(Ok("None".to_string()), |v| Ok(v.str()?.to_string()))
        };
        Ok(Self { tag: text(tag)?, value: text(value)? })
    }

    fn __repr__(&self) -> String {
        format!("TagValue(tag='{}', value='{}')", self.tag, self.value)
    }
}

// ── OrderAllocation (per-account allocation in OrderState) ──

/// ibapi-compatible OrderAllocation class. The quantities are the official
/// API's Decimals (`decimal.Decimal`, unset `UNSET_DECIMAL`), carried as
/// their text (empty when unset).
#[pyclass(from_py_object)]
#[derive(Clone, Default)]
pub struct OrderAllocation {
    #[pyo3(get, set)] pub account: String,
    pub position: String,
    pub position_desired: String,
    pub position_after: String,
    pub desired_alloc_qty: String,
    pub allowed_alloc_qty: String,
    #[pyo3(get, set)] pub is_monetary: bool,
}

#[pymethods]
impl OrderAllocation {
    #[new]
    #[pyo3(signature = ())]
    fn new() -> Self { Self::default() }

    fn __repr__(&self) -> String {
        format!("OrderAllocation(account='{}', position={})", self.account, self.position)
    }

    #[getter(position)]
    fn get_position(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_text_to_py(py, &self.position) }
    #[setter(position)]
    fn set_position(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.position = decimal_text_from_py(v)?; Ok(()) }
    #[getter(position_desired)]
    fn get_position_desired(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_text_to_py(py, &self.position_desired) }
    #[setter(position_desired)]
    fn set_position_desired(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.position_desired = decimal_text_from_py(v)?; Ok(()) }
    #[getter(position_after)]
    fn get_position_after(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_text_to_py(py, &self.position_after) }
    #[setter(position_after)]
    fn set_position_after(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.position_after = decimal_text_from_py(v)?; Ok(()) }
    #[getter(desired_alloc_qty)]
    fn get_desired_alloc_qty(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_text_to_py(py, &self.desired_alloc_qty) }
    #[setter(desired_alloc_qty)]
    fn set_desired_alloc_qty(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.desired_alloc_qty = decimal_text_from_py(v)?; Ok(()) }
    #[getter(allowed_alloc_qty)]
    fn get_allowed_alloc_qty(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_text_to_py(py, &self.allowed_alloc_qty) }
    #[setter(allowed_alloc_qty)]
    fn set_allowed_alloc_qty(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.allowed_alloc_qty = decimal_text_from_py(v)?; Ok(()) }
}

official_names!(OrderAllocation, [
    ("positionDesired", "position_desired"), ("positionAfter", "position_after"),
    ("desiredAllocQty", "desired_alloc_qty"), ("allowedAllocQty", "allowed_alloc_qty"),
    ("isMonetary", "is_monetary"),
]);

// ── OrderState (for what-if responses) ──

/// ibapi-compatible OrderState class (used in openOrder callback), with the
/// official API's defaults: unset numbers `f64::MAX`, the suggested size an
/// unset Decimal, no allocations (None).
#[pyclass(from_py_object)]
pub struct OrderState {
    #[pyo3(get, set)]
    pub status: String,
    #[pyo3(get, set)]
    pub init_margin_before: String,
    #[pyo3(get, set)]
    pub maint_margin_before: String,
    #[pyo3(get, set)]
    pub equity_with_loan_before: String,
    #[pyo3(get, set)]
    pub init_margin_change: String,
    #[pyo3(get, set)]
    pub maint_margin_change: String,
    #[pyo3(get, set)]
    pub equity_with_loan_change: String,
    #[pyo3(get, set)]
    pub init_margin_after: String,
    #[pyo3(get, set)]
    pub maint_margin_after: String,
    #[pyo3(get, set)]
    pub equity_with_loan_after: String,
    #[pyo3(get, set)]
    pub commission_and_fees: f64,
    #[pyo3(get, set)]
    pub min_commission_and_fees: f64,
    #[pyo3(get, set)]
    pub max_commission_and_fees: f64,
    #[pyo3(get, set)]
    pub commission_and_fees_currency: String,
    #[pyo3(get, set)]
    pub warning_text: String,
    #[pyo3(get, set)]
    pub completed_time: String,
    #[pyo3(get, set)]
    pub completed_status: String,
    // ── ibapi-iso extension: RTH-split margin + allocations ──
    #[pyo3(get, set)] pub margin_currency: String,
    #[pyo3(get, set)] pub init_margin_before_outside_rth: f64,
    #[pyo3(get, set)] pub maint_margin_before_outside_rth: f64,
    #[pyo3(get, set)] pub equity_with_loan_before_outside_rth: f64,
    #[pyo3(get, set)] pub init_margin_change_outside_rth: f64,
    #[pyo3(get, set)] pub maint_margin_change_outside_rth: f64,
    #[pyo3(get, set)] pub equity_with_loan_change_outside_rth: f64,
    #[pyo3(get, set)] pub init_margin_after_outside_rth: f64,
    #[pyo3(get, set)] pub maint_margin_after_outside_rth: f64,
    #[pyo3(get, set)] pub equity_with_loan_after_outside_rth: f64,
    /// The official API's Decimal, carried as its text (empty when unset).
    pub suggested_size: String,
    #[pyo3(get, set)] pub reject_reason: String,
    /// `OrderAllocation` items, or None.
    #[pyo3(get, set)] pub order_allocations: Py<PyAny>,
}

impl Clone for OrderState {
    fn clone(&self) -> Self {
        Self {
            order_allocations: Python::attach(|py| self.order_allocations.clone_ref(py)),
            status: self.status.clone(),
            init_margin_before: self.init_margin_before.clone(),
            maint_margin_before: self.maint_margin_before.clone(),
            equity_with_loan_before: self.equity_with_loan_before.clone(),
            init_margin_change: self.init_margin_change.clone(),
            maint_margin_change: self.maint_margin_change.clone(),
            equity_with_loan_change: self.equity_with_loan_change.clone(),
            init_margin_after: self.init_margin_after.clone(),
            maint_margin_after: self.maint_margin_after.clone(),
            equity_with_loan_after: self.equity_with_loan_after.clone(),
            commission_and_fees: self.commission_and_fees,
            min_commission_and_fees: self.min_commission_and_fees,
            max_commission_and_fees: self.max_commission_and_fees,
            commission_and_fees_currency: self.commission_and_fees_currency.clone(),
            warning_text: self.warning_text.clone(),
            completed_time: self.completed_time.clone(),
            completed_status: self.completed_status.clone(),
            margin_currency: self.margin_currency.clone(),
            init_margin_before_outside_rth: self.init_margin_before_outside_rth,
            maint_margin_before_outside_rth: self.maint_margin_before_outside_rth,
            equity_with_loan_before_outside_rth: self.equity_with_loan_before_outside_rth,
            init_margin_change_outside_rth: self.init_margin_change_outside_rth,
            maint_margin_change_outside_rth: self.maint_margin_change_outside_rth,
            equity_with_loan_change_outside_rth: self.equity_with_loan_change_outside_rth,
            init_margin_after_outside_rth: self.init_margin_after_outside_rth,
            maint_margin_after_outside_rth: self.maint_margin_after_outside_rth,
            equity_with_loan_after_outside_rth: self.equity_with_loan_after_outside_rth,
            suggested_size: self.suggested_size.clone(),
            reject_reason: self.reject_reason.clone(),
        }
    }
}

impl Default for OrderState {
    fn default() -> Self {
        Self {
            status: String::new(),
            init_margin_before: String::new(),
            maint_margin_before: String::new(),
            equity_with_loan_before: String::new(),
            init_margin_change: String::new(),
            maint_margin_change: String::new(),
            equity_with_loan_change: String::new(),
            init_margin_after: String::new(),
            maint_margin_after: String::new(),
            equity_with_loan_after: String::new(),
            commission_and_fees: f64::MAX,
            min_commission_and_fees: f64::MAX,
            max_commission_and_fees: f64::MAX,
            commission_and_fees_currency: String::new(),
            warning_text: String::new(),
            completed_time: String::new(),
            completed_status: String::new(),
            margin_currency: String::new(),
            init_margin_before_outside_rth: f64::MAX,
            maint_margin_before_outside_rth: f64::MAX,
            equity_with_loan_before_outside_rth: f64::MAX,
            init_margin_change_outside_rth: f64::MAX,
            maint_margin_change_outside_rth: f64::MAX,
            equity_with_loan_change_outside_rth: f64::MAX,
            init_margin_after_outside_rth: f64::MAX,
            maint_margin_after_outside_rth: f64::MAX,
            equity_with_loan_after_outside_rth: f64::MAX,
            suggested_size: String::new(),
            reject_reason: String::new(),
            order_allocations: Python::attach(|py| py.None()),
        }
    }
}

impl OrderState {
    /// The order state a callback shows, from the Rust API order state.
    pub fn from_api(py: Python<'_>, s: &crate::api::types::OrderState) -> PyResult<Self> {
        let allocations = s.order_allocations.iter().map(|a| Py::new(py, OrderAllocation {
            account: a.account.clone(),
            position: a.position.clone(),
            position_desired: a.position_desired.clone(),
            position_after: a.position_after.clone(),
            desired_alloc_qty: a.desired_alloc_qty.clone(),
            allowed_alloc_qty: a.allowed_alloc_qty.clone(),
            is_monetary: a.is_monetary,
        })).collect::<PyResult<Vec<_>>>()?;
        Ok(Self {
            status: s.status.clone(),
            init_margin_before: s.init_margin_before.clone(),
            maint_margin_before: s.maint_margin_before.clone(),
            equity_with_loan_before: s.equity_with_loan_before.clone(),
            init_margin_change: s.init_margin_change.clone(),
            maint_margin_change: s.maint_margin_change.clone(),
            equity_with_loan_change: s.equity_with_loan_change.clone(),
            init_margin_after: s.init_margin_after.clone(),
            maint_margin_after: s.maint_margin_after.clone(),
            equity_with_loan_after: s.equity_with_loan_after.clone(),
            commission_and_fees: s.commission_and_fees,
            min_commission_and_fees: s.min_commission_and_fees,
            max_commission_and_fees: s.max_commission_and_fees,
            commission_and_fees_currency: s.commission_and_fees_currency.clone(),
            warning_text: s.warning_text.clone(),
            completed_time: s.completed_time.clone(),
            completed_status: s.completed_status.clone(),
            margin_currency: s.margin_currency.clone(),
            init_margin_before_outside_rth: s.init_margin_before_outside_rth,
            maint_margin_before_outside_rth: s.maint_margin_before_outside_rth,
            equity_with_loan_before_outside_rth: s.equity_with_loan_before_outside_rth,
            init_margin_change_outside_rth: s.init_margin_change_outside_rth,
            maint_margin_change_outside_rth: s.maint_margin_change_outside_rth,
            equity_with_loan_change_outside_rth: s.equity_with_loan_change_outside_rth,
            init_margin_after_outside_rth: s.init_margin_after_outside_rth,
            maint_margin_after_outside_rth: s.maint_margin_after_outside_rth,
            equity_with_loan_after_outside_rth: s.equity_with_loan_after_outside_rth,
            suggested_size: s.suggested_size.clone(),
            reject_reason: s.reject_reason.clone(),
            order_allocations: list_or_none(py, allocations)?,
        })
    }
}

#[pymethods]
impl OrderState {
    #[new]
    #[pyo3(signature = ())]
    fn new() -> Self {
        Self::default()
    }

    fn __repr__(&self) -> String {
        format!("OrderState(status='{}')", self.status)
    }

    #[getter(suggested_size)]
    fn get_suggested_size(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_text_to_py(py, &self.suggested_size) }
    #[setter(suggested_size)]
    fn set_suggested_size(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.suggested_size = decimal_text_from_py(v)?; Ok(()) }
}

official_names!(OrderState, [
    ("initMarginBefore", "init_margin_before"), ("maintMarginBefore", "maint_margin_before"),
    ("equityWithLoanBefore", "equity_with_loan_before"), ("initMarginChange", "init_margin_change"),
    ("maintMarginChange", "maint_margin_change"), ("equityWithLoanChange", "equity_with_loan_change"),
    ("initMarginAfter", "init_margin_after"), ("maintMarginAfter", "maint_margin_after"),
    ("equityWithLoanAfter", "equity_with_loan_after"), ("commissionAndFees", "commission_and_fees"),
    ("minCommissionAndFees", "min_commission_and_fees"), ("maxCommissionAndFees", "max_commission_and_fees"),
    ("commissionAndFeesCurrency", "commission_and_fees_currency"), ("marginCurrency", "margin_currency"),
    ("initMarginBeforeOutsideRTH", "init_margin_before_outside_rth"),
    ("maintMarginBeforeOutsideRTH", "maint_margin_before_outside_rth"),
    ("equityWithLoanBeforeOutsideRTH", "equity_with_loan_before_outside_rth"),
    ("initMarginChangeOutsideRTH", "init_margin_change_outside_rth"),
    ("maintMarginChangeOutsideRTH", "maint_margin_change_outside_rth"),
    ("equityWithLoanChangeOutsideRTH", "equity_with_loan_change_outside_rth"),
    ("initMarginAfterOutsideRTH", "init_margin_after_outside_rth"),
    ("maintMarginAfterOutsideRTH", "maint_margin_after_outside_rth"),
    ("equityWithLoanAfterOutsideRTH", "equity_with_loan_after_outside_rth"),
    ("suggestedSize", "suggested_size"), ("rejectReason", "reject_reason"),
    ("orderAllocations", "order_allocations"), ("warningText", "warning_text"),
    ("completedTime", "completed_time"), ("completedStatus", "completed_status"),
]);

// ── Order Conditions ──
//
// As the official API's classes (ibapi 10.46 `order_condition`): the same
// constructor arguments, the camelCase names (`conId`, `isMore`, ...), the
// type in `condType`, every value None until set. A value left None is
// sent as absent, as the official client leaves it out: 0, empty or false.

/// Price condition: trigger when an instrument's price crosses a threshold.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct PriceCondition {
    #[pyo3(get, set)]
    pub con_id: Option<i64>,
    #[pyo3(get, set)]
    pub exchange: Option<String>,
    #[pyo3(get, set)]
    pub price: Option<f64>,
    #[pyo3(get, set)]
    pub is_more: Option<bool>,
    #[pyo3(get, set)]
    pub trigger_method: Option<i32>,
}

#[pymethods]
impl PriceCondition {
    #[new]
    #[pyo3(signature = (triggerMethod=None, conId=None, exch=None, isMore=None, price=None))]
    #[allow(non_snake_case)]
    fn new(triggerMethod: Option<i32>, conId: Option<i64>, exch: Option<String>, isMore: Option<bool>, price: Option<f64>) -> Self {
        Self { con_id: conId, exchange: exch, price, is_more: isMore, trigger_method: triggerMethod }
    }

    #[classattr]
    #[pyo3(name = "condType")]
    fn cond_type() -> i32 { 1 }

    fn __repr__(&self) -> String {
        let op = if self.is_more.unwrap_or(false) { ">" } else { "<" };
        format!("PriceCondition(conId={:?}, price {} {:?})", self.con_id, op, self.price)
    }
}

official_names!(PriceCondition, [
    ("conId", "con_id"), ("isMore", "is_more"), ("triggerMethod", "trigger_method"),
]);

impl PriceCondition {
    pub fn to_internal(&self) -> OrderCondition {
        OrderCondition::Price {
            con_id: self.con_id.unwrap_or(0),
            exchange: self.exchange.clone().unwrap_or_default(),
            price: crate::api::types::price_from_f64(self.price.unwrap_or(0.0)),
            is_more: self.is_more.unwrap_or(false),
            trigger_method: self.trigger_method.unwrap_or(0) as u8,
        }
    }
}

/// Time condition: trigger at a specific time.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct TimeCondition {
    #[pyo3(get, set)]
    pub time: Option<String>,
    #[pyo3(get, set)]
    pub is_more: Option<bool>,
}

#[pymethods]
impl TimeCondition {
    #[new]
    #[pyo3(signature = (isMore=None, time=None))]
    #[allow(non_snake_case)]
    fn new(isMore: Option<bool>, time: Option<String>) -> Self {
        Self { time, is_more: isMore }
    }

    #[classattr]
    #[pyo3(name = "condType")]
    fn cond_type() -> i32 { 3 }

    fn __repr__(&self) -> String {
        let op = if self.is_more.unwrap_or(false) { ">" } else { "<" };
        format!("TimeCondition(time {} {:?})", op, self.time)
    }
}

official_names!(TimeCondition, [("isMore", "is_more")]);

impl TimeCondition {
    pub fn to_internal(&self) -> OrderCondition {
        OrderCondition::Time { time: self.time.clone().unwrap_or_default(), is_more: self.is_more.unwrap_or(false) }
    }
}

/// Margin condition: trigger based on margin cushion percentage.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct MarginCondition {
    #[pyo3(get, set)]
    pub percent: Option<u32>,
    #[pyo3(get, set)]
    pub is_more: Option<bool>,
}

#[pymethods]
impl MarginCondition {
    #[new]
    #[pyo3(signature = (isMore=None, percent=None))]
    #[allow(non_snake_case)]
    fn new(isMore: Option<bool>, percent: Option<u32>) -> Self {
        Self { percent, is_more: isMore }
    }

    #[classattr]
    #[pyo3(name = "condType")]
    fn cond_type() -> i32 { 4 }

    fn __repr__(&self) -> String {
        format!("MarginCondition({:?}% {})", self.percent, if self.is_more.unwrap_or(false) { "above" } else { "below" })
    }
}

official_names!(MarginCondition, [("isMore", "is_more")]);

impl MarginCondition {
    pub fn to_internal(&self) -> OrderCondition {
        OrderCondition::Margin { percent: self.percent.unwrap_or(0), is_more: self.is_more.unwrap_or(false) }
    }
}

/// Execution condition: trigger on trade execution.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct ExecutionCondition {
    #[pyo3(get, set)]
    pub symbol: Option<String>,
    #[pyo3(get, set)]
    pub exchange: Option<String>,
    #[pyo3(get, set)]
    pub sec_type: Option<String>,
}

#[pymethods]
impl ExecutionCondition {
    #[new]
    #[pyo3(signature = (secType=None, exch=None, symbol=None))]
    #[allow(non_snake_case)]
    fn new(secType: Option<String>, exch: Option<String>, symbol: Option<String>) -> Self {
        Self { symbol, exchange: exch, sec_type: secType }
    }

    #[classattr]
    #[pyo3(name = "condType")]
    fn cond_type() -> i32 { 5 }

    fn __repr__(&self) -> String {
        format!("ExecutionCondition(symbol={:?}, exchange={:?})", self.symbol, self.exchange)
    }
}

official_names!(ExecutionCondition, [("secType", "sec_type")]);

impl ExecutionCondition {
    pub fn to_internal(&self) -> OrderCondition {
        OrderCondition::Execution {
            symbol: self.symbol.clone().unwrap_or_default(),
            exchange: self.exchange.clone().unwrap_or_default(),
            sec_type: self.sec_type.clone().unwrap_or_default(),
        }
    }
}

/// Volume condition: trigger when volume exceeds a threshold.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct VolumeCondition {
    #[pyo3(get, set)]
    pub con_id: Option<i64>,
    #[pyo3(get, set)]
    pub exchange: Option<String>,
    #[pyo3(get, set)]
    pub volume: Option<i64>,
    #[pyo3(get, set)]
    pub is_more: Option<bool>,
}

#[pymethods]
impl VolumeCondition {
    #[new]
    #[pyo3(signature = (conId=None, exch=None, isMore=None, volume=None))]
    #[allow(non_snake_case)]
    fn new(conId: Option<i64>, exch: Option<String>, isMore: Option<bool>, volume: Option<i64>) -> Self {
        Self { con_id: conId, exchange: exch, volume, is_more: isMore }
    }

    #[classattr]
    #[pyo3(name = "condType")]
    fn cond_type() -> i32 { 6 }

    fn __repr__(&self) -> String {
        let op = if self.is_more.unwrap_or(false) { ">" } else { "<" };
        format!("VolumeCondition(conId={:?}, volume {} {:?})", self.con_id, op, self.volume)
    }
}

official_names!(VolumeCondition, [("conId", "con_id"), ("isMore", "is_more")]);

impl VolumeCondition {
    pub fn to_internal(&self) -> OrderCondition {
        OrderCondition::Volume {
            con_id: self.con_id.unwrap_or(0),
            exchange: self.exchange.clone().unwrap_or_default(),
            volume: self.volume.unwrap_or(0),
            is_more: self.is_more.unwrap_or(false),
        }
    }
}

/// Percentage change condition: trigger on % change from close. The change
/// is unset (`f64::MAX`) by default, as the official API's.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct PercentChangeCondition {
    #[pyo3(get, set)]
    pub con_id: Option<i64>,
    #[pyo3(get, set)]
    pub exchange: Option<String>,
    #[pyo3(get, set)]
    pub change_percent: f64,
    #[pyo3(get, set)]
    pub is_more: Option<bool>,
}

#[pymethods]
impl PercentChangeCondition {
    #[new]
    #[pyo3(signature = (conId=None, exch=None, isMore=None, changePercent=f64::MAX))]
    #[allow(non_snake_case)]
    fn new(conId: Option<i64>, exch: Option<String>, isMore: Option<bool>, changePercent: f64) -> Self {
        Self { con_id: conId, exchange: exch, change_percent: changePercent, is_more: isMore }
    }

    #[classattr]
    #[pyo3(name = "condType")]
    fn cond_type() -> i32 { 7 }

    fn __repr__(&self) -> String {
        let op = if self.is_more.unwrap_or(false) { ">" } else { "<" };
        format!("PercentChangeCondition(conId={:?}, {}% {})", self.con_id, op, self.change_percent)
    }
}

official_names!(PercentChangeCondition, [
    ("conId", "con_id"), ("isMore", "is_more"), ("changePercent", "change_percent"),
]);

impl PercentChangeCondition {
    pub fn to_internal(&self) -> OrderCondition {
        OrderCondition::PercentChange {
            con_id: self.con_id.unwrap_or(0),
            exchange: self.exchange.clone().unwrap_or_default(),
            percent: if self.change_percent == f64::MAX { 0.0 } else { self.change_percent },
            is_more: self.is_more.unwrap_or(false),
        }
    }
}

// ── BarData ──

/// ibapi-compatible BarData class for historical data callbacks.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct BarData {
    #[pyo3(get, set)]
    pub date: String,
    #[pyo3(get, set)]
    pub open: f64,
    #[pyo3(get, set)]
    pub high: f64,
    #[pyo3(get, set)]
    pub low: f64,
    #[pyo3(get, set)]
    pub close: f64,
    /// The official API's Decimal (`decimal.Decimal`), unset `f64::MAX`.
    pub volume: f64,
    /// The official API's Decimal (`decimal.Decimal`), unset `f64::MAX`.
    pub wap: f64,
    #[pyo3(get, set)]
    pub bar_count: i32,
    /// Timezone of `date` as reported by the reply (ibx#234) — previously
    /// parsed and then discarded, leaving the bare timestamp string as the
    /// only (unverifiable) evidence of what the bar times mean. Empty on
    /// streaming updates, which carry no timezone of their own.
    #[pyo3(get, set)]
    pub timezone: String,
}

impl BarData {
    /// A bar a callback shows.
    #[allow(clippy::too_many_arguments)]
    pub fn reported(date: String, open: f64, high: f64, low: f64, close: f64, volume: i64, wap: f64, bar_count: i32, timezone: String) -> Self {
        Self { date, open, high, low, close, volume: volume as f64, wap, bar_count, timezone }
    }
}

#[pymethods]
impl BarData {
    /// The official API's defaults: the volume and the WAP unset.
    #[new]
    #[pyo3(signature = (date="".to_string(), open=0.0, high=0.0, low=0.0, close=0.0, volume=None, wap=None, bar_count=0, timezone="".to_string()))]
    #[allow(clippy::too_many_arguments)]
    fn new(date: String, open: f64, high: f64, low: f64, close: f64, volume: Option<&Bound<'_, PyAny>>, wap: Option<&Bound<'_, PyAny>>, bar_count: i32, timezone: String) -> PyResult<Self> {
        let decimal = |v: Option<&Bound<'_, PyAny>>| -> PyResult<f64> { Ok(v.map(decimal_from_py).transpose()?.unwrap_or(f64::MAX)) };
        Ok(Self { date, open, high, low, close, volume: decimal(volume)?, wap: decimal(wap)?, bar_count, timezone })
    }

    #[getter(volume)]
    fn get_volume(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.volume) }
    #[setter(volume)]
    fn set_volume(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.volume = decimal_from_py(v)?; Ok(()) }
    #[getter(wap)]
    fn get_wap(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.wap) }
    #[setter(wap)]
    fn set_wap(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.wap = decimal_from_py(v)?; Ok(()) }

    fn __repr__(&self) -> String {
        format!("BarData(date='{}', O={}, H={}, L={}, C={}, V={})",
            self.date, self.open, self.high, self.low, self.close, self.volume)
    }

    // ibapi camelCase aliases (ibx#487: the official client library's names)
    #[getter(barCount)]
    fn get_bar_count_alias(&self) -> i32 { self.bar_count }
    #[setter(barCount)]
    fn set_bar_count_alias(&mut self, v: i32) { self.bar_count = v; }
}

// ── ContractDetails ──

/// ibapi-compatible IneligibilityReason class (ibx#436). As the official
/// API's, the constructor keeps the text of what it is given:
/// `IneligibilityReason()` has the id and description 'None'.
#[pyclass(from_py_object, name = "IneligibilityReason")]
#[derive(Clone, Debug, Default)]
pub struct IneligibilityReasonPy {
    #[pyo3(get, set)]
    pub id_: String,
    #[pyo3(get, set)]
    pub description: String,
}

#[pymethods]
impl IneligibilityReasonPy {
    #[new]
    #[pyo3(signature = (id_=None, description=None))]
    fn new(id_: Option<&Bound<'_, PyAny>>, description: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let text = |v: Option<&Bound<'_, PyAny>>| -> PyResult<String> {
            v.map_or(Ok("None".to_string()), |v| Ok(v.str()?.to_string()))
        };
        Ok(Self { id_: text(id_)?, description: text(description)? })
    }

    fn __repr__(&self) -> String {
        format!("IneligibilityReason(id_='{}', description='{}')", self.id_, self.description)
    }
}

/// Generates the ibapi-compatible ContractDetails class: every field but
/// the contract and the two lists, with its clone and its default.
macro_rules! contract_details_class {
    ($($field:ident: $ty:ty = $default:expr),* $(,)?) => {
        /// ibapi-compatible ContractDetails class, with the fields of a
        /// contract row and of a bond row (ibx#436). The sizes are the
        /// official API's Decimals (`decimal.Decimal`, unset
        /// `UNSET_DECIMAL`; `f64::MAX` here).
        #[pyclass(from_py_object)]
        pub struct ContractDetails {
            /// Stored as `Py<Contract>` so the getter hands Python THE contained
            /// object, not a copy: with a plain field, `details.contract.con_id = x`
            /// mutated a temporary clone and was a silent no-op (ibx#230).
            #[pyo3(get, set)]
            pub contract: Py<Contract>,
            /// `TagValue` items (ISIN, CUSIP), None when there is none.
            #[pyo3(get, set)]
            pub sec_id_list: Py<PyAny>,
            /// `IneligibilityReason` items, None when there is none.
            #[pyo3(get, set)]
            pub ineligibility_reason_list: Py<PyAny>,
            pub min_size: f64,
            pub size_increment: f64,
            pub suggested_size_increment: f64,
            pub min_algo_size: f64,
            pub last_price_precision: f64,
            pub last_size_precision: f64,
            $(
                #[pyo3(get, set)]
                pub $field: $ty,
            )*
        }

        impl Clone for ContractDetails {
            /// `Py<Contract>` clones by reference under the GIL: the copy shares the
            /// same Python Contract object, matching Python assignment semantics.
            fn clone(&self) -> Self {
                Python::attach(|py| Self {
                    contract: self.contract.clone_ref(py),
                    sec_id_list: self.sec_id_list.clone_ref(py),
                    ineligibility_reason_list: self.ineligibility_reason_list.clone_ref(py),
                    min_size: self.min_size,
                    size_increment: self.size_increment,
                    suggested_size_increment: self.suggested_size_increment,
                    min_algo_size: self.min_algo_size,
                    last_price_precision: self.last_price_precision,
                    last_size_precision: self.last_size_precision,
                    $($field: self.$field.clone(),)*
                })
            }
        }

        impl ContractDetails {
            /// Fresh instance with an owned default Contract. `Py<Contract>` has no
            /// Default, so this replaces the derived constructor (ibx#230).
            pub fn new_default(py: Python<'_>) -> Self {
                Self {
                    contract: Py::new(py, Contract::default()).expect("Contract allocation failed"),
                    sec_id_list: py.None(),
                    ineligibility_reason_list: py.None(),
                    min_size: f64::MAX,
                    size_increment: f64::MAX,
                    suggested_size_increment: f64::MAX,
                    min_algo_size: f64::MAX,
                    last_price_precision: f64::MAX,
                    last_size_precision: f64::MAX,
                    $($field: $default,)*
                }
            }
        }
    };
}

contract_details_class! {
    market_name: String = String::new(),
    min_tick: f64 = 0.0,
    order_types: String = String::new(),
    valid_exchanges: String = String::new(),
    price_magnifier: i32 = 0,
    under_con_id: i32 = 0,
    long_name: String = String::new(),
    contract_month: String = String::new(),
    industry: String = String::new(),
    category: String = String::new(),
    subcategory: String = String::new(),
    time_zone_id: String = String::new(),
    trading_hours: String = String::new(),
    liquid_hours: String = String::new(),
    ev_rule: String = String::new(),
    ev_multiplier: f64 = 0.0,
    agg_group: i32 = 0,
    under_symbol: String = String::new(),
    under_sec_type: String = String::new(),
    market_rule_ids: String = String::new(),
    real_expiration_date: String = String::new(),
    last_trade_time: String = String::new(),
    stock_type: String = String::new(),
    cusip: String = String::new(),
    ratings: String = String::new(),
    desc_append: String = String::new(),
    bond_type: String = String::new(),
    coupon_type: String = String::new(),
    callable: bool = false,
    putable: bool = false,
    coupon: f64 = 0.0,
    convertible: bool = false,
    maturity: String = String::new(),
    issue_date: String = String::new(),
    next_option_date: String = String::new(),
    next_option_type: String = String::new(),
    next_option_partial: bool = false,
    notes: String = String::new(),
    fund_name: String = String::new(),
    fund_family: String = String::new(),
    fund_type: String = String::new(),
    fund_front_load: String = String::new(),
    fund_back_load: String = String::new(),
    fund_back_load_time_interval: String = String::new(),
    fund_management_fee: String = String::new(),
    fund_closed: bool = false,
    fund_closed_for_new_investors: bool = false,
    fund_closed_for_new_money: bool = false,
    fund_notify_amount: String = String::new(),
    fund_minimum_initial_purchase: String = String::new(),
    fund_subsequent_minimum_purchase: String = String::new(),
    fund_blue_sky_states: String = String::new(),
    fund_blue_sky_territories: String = String::new(),
    fund_distribution_policy_indicator: String = String::new(),
    fund_asset_type: String = String::new(),
    event_contract1: String = String::new(),
    event_contract_description1: String = String::new(),
    event_contract_description2: String = String::new(),
}

#[pymethods]
impl ContractDetails {
    #[new]
    #[pyo3(signature = ())]
    fn py_new(py: Python<'_>) -> Self {
        Self::new_default(py)
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        format!("ContractDetails(symbol='{}', longName='{}')",
            self.contract.borrow(py).symbol, self.long_name)
    }

    #[getter(min_size)]
    fn get_min_size(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.min_size) }
    #[setter(min_size)]
    fn set_min_size(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.min_size = decimal_from_py(v)?; Ok(()) }
    #[getter(size_increment)]
    fn get_size_increment(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.size_increment) }
    #[setter(size_increment)]
    fn set_size_increment(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.size_increment = decimal_from_py(v)?; Ok(()) }
    #[getter(suggested_size_increment)]
    fn get_suggested_size_increment(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.suggested_size_increment) }
    #[setter(suggested_size_increment)]
    fn set_suggested_size_increment(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.suggested_size_increment = decimal_from_py(v)?; Ok(()) }
    #[getter(min_algo_size)]
    fn get_min_algo_size(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.min_algo_size) }
    #[setter(min_algo_size)]
    fn set_min_algo_size(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.min_algo_size = decimal_from_py(v)?; Ok(()) }
    #[getter(last_price_precision)]
    fn get_last_price_precision(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.last_price_precision) }
    #[setter(last_price_precision)]
    fn set_last_price_precision(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.last_price_precision = decimal_from_py(v)?; Ok(()) }
    #[getter(last_size_precision)]
    fn get_last_size_precision(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.last_size_precision) }
    #[setter(last_size_precision)]
    fn set_last_size_precision(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.last_size_precision = decimal_from_py(v)?; Ok(()) }
}

official_names!(ContractDetails, [
    ("marketName", "market_name"), ("minTick", "min_tick"), ("orderTypes", "order_types"),
    ("validExchanges", "valid_exchanges"), ("priceMagnifier", "price_magnifier"), ("underConId", "under_con_id"),
    ("longName", "long_name"), ("contractMonth", "contract_month"), ("timeZoneId", "time_zone_id"),
    ("tradingHours", "trading_hours"), ("liquidHours", "liquid_hours"), ("evRule", "ev_rule"),
    ("evMultiplier", "ev_multiplier"), ("aggGroup", "agg_group"), ("underSymbol", "under_symbol"),
    ("underSecType", "under_sec_type"), ("marketRuleIds", "market_rule_ids"), ("secIdList", "sec_id_list"),
    ("realExpirationDate", "real_expiration_date"), ("lastTradeTime", "last_trade_time"), ("stockType", "stock_type"),
    ("minSize", "min_size"), ("sizeIncrement", "size_increment"), ("suggestedSizeIncrement", "suggested_size_increment"),
    ("minAlgoSize", "min_algo_size"), ("lastPricePrecision", "last_price_precision"),
    ("lastSizePrecision", "last_size_precision"), ("descAppend", "desc_append"), ("bondType", "bond_type"),
    ("couponType", "coupon_type"), ("issueDate", "issue_date"), ("nextOptionDate", "next_option_date"),
    ("nextOptionType", "next_option_type"), ("nextOptionPartial", "next_option_partial"), ("fundName", "fund_name"),
    ("fundFamily", "fund_family"), ("fundType", "fund_type"), ("fundFrontLoad", "fund_front_load"),
    ("fundBackLoad", "fund_back_load"), ("fundBackLoadTimeInterval", "fund_back_load_time_interval"),
    ("fundManagementFee", "fund_management_fee"), ("fundClosed", "fund_closed"),
    ("fundClosedForNewInvestors", "fund_closed_for_new_investors"), ("fundClosedForNewMoney", "fund_closed_for_new_money"),
    ("fundNotifyAmount", "fund_notify_amount"), ("fundMinimumInitialPurchase", "fund_minimum_initial_purchase"),
    ("fundSubsequentMinimumPurchase", "fund_subsequent_minimum_purchase"), ("fundBlueSkyStates", "fund_blue_sky_states"),
    ("fundBlueSkyTerritories", "fund_blue_sky_territories"),
    ("fundDistributionPolicyIndicator", "fund_distribution_policy_indicator"), ("fundAssetType", "fund_asset_type"),
    ("ineligibilityReasonList", "ineligibility_reason_list"), ("eventContract1", "event_contract1"),
    ("eventContractDescription1", "event_contract_description1"),
    ("eventContractDescription2", "event_contract_description2"),
]);

impl ContractDetails {
    /// The row of a definition: the fields of the Rust API row (ibx#436).
    pub fn from_definition(py: Python<'_>, def: &crate::control::contracts::ContractDefinition) -> Self {
        let d = crate::api::types::ContractDetails::from_definition(def);
        let r = &d.contract;
        let c = Contract {
            con_id: r.con_id,
            symbol: r.symbol.clone(),
            sec_type: r.sec_type.clone(),
            exchange: r.exchange.clone(),
            primary_exchange: r.primary_exchange.clone(),
            currency: r.currency.clone(),
            local_symbol: r.local_symbol.clone(),
            trading_class: r.trading_class.clone(),
            last_trade_date_or_contract_month: r.last_trade_date_or_contract_month.clone(),
            last_trade_date: r.last_trade_date.clone(),
            strike: r.strike,
            right: r.right.clone(),
            multiplier: r.multiplier.clone(),
            ..Default::default()
        };
        let sec_id_list = d.sec_id_list.iter()
            .map(|tv| Py::new(py, TagValue { tag: tv.tag.clone(), value: tv.value.clone() }).expect("TagValue allocation failed").into_any())
            .collect();
        let ineligibility_reason_list = d.ineligibility_reason_list.iter()
            .map(|r| Py::new(py, IneligibilityReasonPy { id_: r.id.clone(), description: r.description.clone() }).expect("IneligibilityReason allocation failed").into_any())
            .collect();
        // None when the row has none, as the official API's decoding.
        let sec_id_list = list_or_none(py, sec_id_list).expect("list allocation failed");
        let ineligibility_reason_list = list_or_none(py, ineligibility_reason_list).expect("list allocation failed");
        Self {
            contract: Py::new(py, c).expect("Contract allocation failed"),
            sec_id_list,
            ineligibility_reason_list,
            market_name: d.market_name,
            min_tick: d.min_tick,
            order_types: d.order_types,
            valid_exchanges: d.valid_exchanges,
            price_magnifier: d.price_magnifier,
            under_con_id: d.under_con_id,
            long_name: d.long_name,
            contract_month: d.contract_month,
            industry: d.industry,
            category: d.category,
            subcategory: d.subcategory,
            time_zone_id: d.time_zone_id,
            trading_hours: d.trading_hours,
            liquid_hours: d.liquid_hours,
            ev_rule: d.ev_rule,
            ev_multiplier: d.ev_multiplier,
            agg_group: d.agg_group,
            under_symbol: d.under_symbol,
            under_sec_type: d.under_sec_type,
            market_rule_ids: d.market_rule_ids,
            real_expiration_date: d.real_expiration_date,
            last_trade_time: d.last_trade_time,
            stock_type: d.stock_type,
            min_size: d.min_size,
            size_increment: d.size_increment,
            suggested_size_increment: d.suggested_size_increment,
            min_algo_size: d.min_algo_size,
            last_price_precision: d.last_price_precision,
            last_size_precision: d.last_size_precision,
            cusip: d.cusip,
            ratings: d.ratings,
            desc_append: d.desc_append,
            bond_type: d.bond_type,
            coupon_type: d.coupon_type,
            callable: d.callable,
            putable: d.putable,
            coupon: d.coupon,
            convertible: d.convertible,
            maturity: d.maturity,
            issue_date: d.issue_date,
            next_option_date: d.next_option_date,
            next_option_type: d.next_option_type,
            next_option_partial: d.next_option_partial,
            notes: d.notes,
            fund_name: d.fund_name,
            fund_family: d.fund_family,
            fund_type: d.fund_type,
            fund_front_load: d.fund_front_load,
            fund_back_load: d.fund_back_load,
            fund_back_load_time_interval: d.fund_back_load_time_interval,
            fund_management_fee: d.fund_management_fee,
            fund_closed: d.fund_closed,
            fund_closed_for_new_investors: d.fund_closed_for_new_investors,
            fund_closed_for_new_money: d.fund_closed_for_new_money,
            fund_notify_amount: d.fund_notify_amount,
            fund_minimum_initial_purchase: d.fund_minimum_initial_purchase,
            fund_subsequent_minimum_purchase: d.fund_subsequent_minimum_purchase,
            fund_blue_sky_states: d.fund_blue_sky_states,
            fund_blue_sky_territories: d.fund_blue_sky_territories,
            fund_distribution_policy_indicator: d.fund_distribution_policy_indicator,
            fund_asset_type: d.fund_asset_type,
            event_contract1: d.event_contract1,
            event_contract_description1: d.event_contract_description1,
            event_contract_description2: d.event_contract_description2,
        }
    }
}

// ── Execution ──

/// ibapi-compatible Execution class (used in exec_details callback). The
/// shares and the cumulative quantity are the official API's Decimals
/// (`decimal.Decimal`, unset `UNSET_DECIMAL`; `f64::MAX` here).
#[pyclass(from_py_object)]
#[derive(Clone, Debug)]
pub struct Execution {
    #[pyo3(get, set)]
    pub exec_id: String,
    #[pyo3(get, set)]
    pub time: String,
    #[pyo3(get, set)]
    pub acct_number: String,
    #[pyo3(get, set)]
    pub exchange: String,
    #[pyo3(get, set)]
    pub side: String,
    pub shares: f64,
    #[pyo3(get, set)]
    pub price: f64,
    #[pyo3(get, set)]
    pub perm_id: i64,
    #[pyo3(get, set)]
    pub client_id: i64,
    #[pyo3(get, set)]
    pub order_id: i64,
    #[pyo3(get, set)]
    pub liquidation: i32,
    pub cum_qty: f64,
    #[pyo3(get, set)]
    pub avg_price: f64,
    #[pyo3(get, set)]
    pub order_ref: String,
    #[pyo3(get, set)]
    pub ev_rule: String,
    #[pyo3(get, set)]
    pub ev_multiplier: f64,
    #[pyo3(get, set)]
    pub model_code: String,
    #[pyo3(get, set)]
    pub last_liquidity: i32,
    #[pyo3(get, set)]
    pub pending_price_revision: bool,
    #[pyo3(get, set)]
    pub submitter: String,
}

impl Default for Execution {
    fn default() -> Self {
        Self {
            exec_id: String::new(),
            time: String::new(),
            acct_number: String::new(),
            exchange: String::new(),
            side: String::new(),
            shares: f64::MAX,
            price: 0.0,
            perm_id: 0,
            client_id: 0,
            order_id: 0,
            liquidation: 0,
            cum_qty: f64::MAX,
            avg_price: 0.0,
            order_ref: String::new(),
            ev_rule: String::new(),
            ev_multiplier: 0.0,
            model_code: String::new(),
            last_liquidity: 0,
            pending_price_revision: false,
            submitter: String::new(),
        }
    }
}

#[pymethods]
impl Execution {
    #[new]
    #[pyo3(signature = ())]
    fn new() -> Self { Self::default() }

    #[getter(shares)]
    fn get_shares(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.shares) }
    #[setter(shares)]
    fn set_shares(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.shares = decimal_from_py(v)?; Ok(()) }
    #[getter(cum_qty)]
    fn get_cum_qty(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.cum_qty) }
    #[setter(cum_qty)]
    fn set_cum_qty(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.cum_qty = decimal_from_py(v)?; Ok(()) }

    // ibapi camelCase aliases (ibx#487: the official client library's names)
    #[getter(execId)]
    fn get_exec_id_alias(&self) -> String { self.exec_id.clone() }
    #[setter(execId)]
    fn set_exec_id_alias(&mut self, v: String) { self.exec_id = v; }
    #[getter(acctNumber)]
    fn get_acct_number_alias(&self) -> String { self.acct_number.clone() }
    #[setter(acctNumber)]
    fn set_acct_number_alias(&mut self, v: String) { self.acct_number = v; }
    #[getter(permId)]
    fn get_perm_id_alias(&self) -> i64 { self.perm_id }
    #[setter(permId)]
    fn set_perm_id_alias(&mut self, v: i64) { self.perm_id = v; }
    #[getter(clientId)]
    fn get_client_id_alias(&self) -> i64 { self.client_id }
    #[setter(clientId)]
    fn set_client_id_alias(&mut self, v: i64) { self.client_id = v; }
    #[getter(orderId)]
    fn get_order_id_alias(&self) -> i64 { self.order_id }
    #[setter(orderId)]
    fn set_order_id_alias(&mut self, v: i64) { self.order_id = v; }
    #[getter(cumQty)]
    fn get_cum_qty_alias(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.cum_qty) }
    #[setter(cumQty)]
    fn set_cum_qty_alias(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.cum_qty = decimal_from_py(v)?; Ok(()) }
    #[getter(avgPrice)]
    fn get_avg_price_alias(&self) -> f64 { self.avg_price }
    #[setter(avgPrice)]
    fn set_avg_price_alias(&mut self, v: f64) { self.avg_price = v; }
    #[getter(orderRef)]
    fn get_order_ref_alias(&self) -> String { self.order_ref.clone() }
    #[setter(orderRef)]
    fn set_order_ref_alias(&mut self, v: String) { self.order_ref = v; }
    #[getter(evRule)]
    fn get_ev_rule_alias(&self) -> String { self.ev_rule.clone() }
    #[setter(evRule)]
    fn set_ev_rule_alias(&mut self, v: String) { self.ev_rule = v; }
    #[getter(evMultiplier)]
    fn get_ev_multiplier_alias(&self) -> f64 { self.ev_multiplier }
    #[setter(evMultiplier)]
    fn set_ev_multiplier_alias(&mut self, v: f64) { self.ev_multiplier = v; }
    #[getter(modelCode)]
    fn get_model_code_alias(&self) -> String { self.model_code.clone() }
    #[setter(modelCode)]
    fn set_model_code_alias(&mut self, v: String) { self.model_code = v; }
    #[getter(lastLiquidity)]
    fn get_last_liquidity_alias(&self) -> i32 { self.last_liquidity }
    #[setter(lastLiquidity)]
    fn set_last_liquidity_alias(&mut self, v: i32) { self.last_liquidity = v; }
    #[getter(pendingPriceRevision)]
    fn get_pending_price_revision_alias(&self) -> bool { self.pending_price_revision }
    #[setter(pendingPriceRevision)]
    fn set_pending_price_revision_alias(&mut self, v: bool) { self.pending_price_revision = v; }
}

// ── SmartComponent ──

/// ibapi-compatible SmartComponent class.
#[pyclass(from_py_object, name = "SmartComponent")]
#[derive(Clone, Debug, Default)]
pub struct SmartComponentPy {
    #[pyo3(get, set)]
    pub bit_number: i32,
    #[pyo3(get, set)]
    pub exchange: String,
    #[pyo3(get, set)]
    pub exchange_letter: String,
}

#[pymethods]
impl SmartComponentPy {
    #[new]
    #[pyo3(signature = ())]
    fn new() -> Self { Self::default() }
}

official_names!(SmartComponentPy, [("bitNumber", "bit_number"), ("exchangeLetter", "exchange_letter")]);

// ── NewsProvider ──

/// ibapi-compatible NewsProvider class.
#[pyclass(from_py_object, name = "NewsProvider")]
#[derive(Clone, Debug, Default)]
pub struct NewsProviderPy {
    #[pyo3(get, set)]
    pub code: String,
    #[pyo3(get, set)]
    pub name: String,
}

#[pymethods]
impl NewsProviderPy {
    #[new]
    #[pyo3(signature = ())]
    fn new() -> Self { Self::default() }
}

// ── SoftDollarTier ──

/// ibapi-compatible SoftDollarTier class.
#[pyclass(from_py_object, name = "SoftDollarTier")]
#[derive(Clone, Debug, Default)]
pub struct SoftDollarTierPy {
    #[pyo3(get, set)]
    pub name: String,
    #[pyo3(get, set)]
    pub val: String,
    #[pyo3(get, set)]
    pub display_name: String,
}

#[pymethods]
impl SoftDollarTierPy {
    /// The official API's constructor arguments.
    #[new]
    #[pyo3(signature = (name="".to_string(), val="".to_string(), displayName="".to_string()))]
    #[allow(non_snake_case)]
    fn new(name: String, val: String, displayName: String) -> Self {
        Self { name, val, display_name: displayName }
    }
}

official_names!(SoftDollarTierPy, [("displayName", "display_name")]);

// ── CommissionAndFeesReport ──

/// ibapi-compatible CommissionAndFeesReport class.
#[pyclass(from_py_object)]
#[derive(Clone, Debug, Default)]
pub struct CommissionAndFeesReport {
    #[pyo3(get, set)]
    pub exec_id: String,
    #[pyo3(get, set)]
    pub commission_and_fees: f64,
    #[pyo3(get, set)]
    pub currency: String,
    #[pyo3(get, set)]
    pub realized_pnl: f64,
    #[pyo3(get, set)]
    pub yield_amount: f64,
    /// `YYYYMMDD`; an int in Python, 0 when none, as the official API's.
    pub yield_redemption_date: String,
}

#[pymethods]
impl CommissionAndFeesReport {
    #[new]
    #[pyo3(signature = ())]
    fn new() -> Self { Self::default() }

    // ibapi camelCase aliases (ibx#487: the official client library's names)
    #[getter(execId)]
    fn get_exec_id_alias(&self) -> String { self.exec_id.clone() }
    #[setter(execId)]
    fn set_exec_id_alias(&mut self, v: String) { self.exec_id = v; }
    #[getter(commissionAndFees)]
    fn get_commission_and_fees_alias(&self) -> f64 { self.commission_and_fees }
    #[setter(commissionAndFees)]
    fn set_commission_and_fees_alias(&mut self, v: f64) { self.commission_and_fees = v; }
    #[getter(realizedPNL)]
    fn get_realized_pnl_alias(&self) -> f64 { self.realized_pnl }
    #[setter(realizedPNL)]
    fn set_realized_pnl_alias(&mut self, v: f64) { self.realized_pnl = v; }
    #[getter(yield_)]
    fn get_yield_amount_alias(&self) -> f64 { self.yield_amount }
    #[setter(yield_)]
    fn set_yield_amount_alias(&mut self, v: f64) { self.yield_amount = v; }
    #[getter(yieldRedemptionDate)]
    fn get_yield_redemption_date_alias(&self) -> i64 { self.yield_redemption_date.parse().unwrap_or(0) }
    #[setter(yieldRedemptionDate)]
    fn set_yield_redemption_date_alias(&mut self, v: i64) { self.yield_redemption_date = if v == 0 { String::new() } else { v.to_string() }; }
    #[getter(yield_redemption_date)]
    fn get_yield_redemption_date(&self) -> i64 { self.yield_redemption_date.parse().unwrap_or(0) }
    #[setter(yield_redemption_date)]
    fn set_yield_redemption_date(&mut self, v: i64) { self.yield_redemption_date = if v == 0 { String::new() } else { v.to_string() }; }
}

// ── ContractDescription ──

/// ibapi-compatible ContractDescription class for symbol search results.
#[pyclass(from_py_object)]
#[derive(Debug, Clone)]
pub struct ContractDescription {
    #[pyo3(get, set)]
    pub con_id: i64,
    #[pyo3(get, set)]
    pub symbol: String,
    #[pyo3(get, set)]
    pub sec_type: String,
    #[pyo3(get, set)]
    pub currency: String,
    #[pyo3(get, set)]
    pub primary_exchange: String,
    #[pyo3(get, set)]
    pub derivative_sec_types: Vec<String>,
    #[pyo3(get, set)]
    pub description: String,
    #[pyo3(get, set)]
    pub issuer_id: String,
}

#[pymethods]
impl ContractDescription {
    #[new]
    #[pyo3(signature = (con_id=0, symbol="".to_string(), sec_type="".to_string(), currency="".to_string(), primary_exchange="".to_string(), derivative_sec_types=Vec::new(), description="".to_string(), issuer_id="".to_string()))]
    #[allow(clippy::too_many_arguments)]
    fn new(con_id: i64, symbol: String, sec_type: String, currency: String, primary_exchange: String, derivative_sec_types: Vec<String>, description: String, issuer_id: String) -> Self {
        Self { con_id, symbol, sec_type, currency, primary_exchange, derivative_sec_types, description, issuer_id }
    }

    fn __repr__(&self) -> String {
        format!("ContractDescription(conId={}, symbol='{}', secType='{}', currency='{}')",
            self.con_id, self.symbol, self.sec_type, self.currency)
    }

    /// The official API's shape: the described contract.
    #[getter(contract)]
    fn get_contract(&self) -> Contract {
        Contract {
            con_id: self.con_id,
            symbol: self.symbol.clone(),
            sec_type: self.sec_type.clone(),
            currency: self.currency.clone(),
            primary_exchange: self.primary_exchange.clone(),
            description: self.description.clone(),
            issuer_id: self.issuer_id.clone(),
            ..Default::default()
        }
    }
}

official_names!(ContractDescription, [("derivativeSecTypes", "derivative_sec_types")]);

#[pyclass(from_py_object, name = "DepthMktDataDescription")]
#[derive(Debug, Clone)]
pub struct DepthMktDataDescriptionPy {
    #[pyo3(get, set)]
    pub exchange: String,
    #[pyo3(get, set)]
    pub sec_type: String,
    #[pyo3(get, set)]
    pub listing_exch: String,
    #[pyo3(get, set)]
    pub service_data_type: String,
    #[pyo3(get, set)]
    pub agg_group: i32,
}

#[pymethods]
impl DepthMktDataDescriptionPy {
    /// The official API's defaults: the aggregation group unset.
    #[new]
    #[pyo3(signature = (exchange="".to_string(), sec_type="".to_string(), listing_exch="".to_string(), service_data_type="".to_string(), agg_group=i32::MAX))]
    fn new(exchange: String, sec_type: String, listing_exch: String, service_data_type: String, agg_group: i32) -> Self {
        Self { exchange, sec_type, listing_exch, service_data_type, agg_group }
    }

    fn __repr__(&self) -> String {
        format!("DepthMktDataDescription(exchange='{}', secType='{}', listingExch='{}', serviceDataType='{}', aggGroup={})",
            self.exchange, self.sec_type, self.listing_exch, self.service_data_type, self.agg_group)
    }
}

official_names!(DepthMktDataDescriptionPy, [
    ("secType", "sec_type"), ("listingExch", "listing_exch"), ("serviceDataType", "service_data_type"),
    ("aggGroup", "agg_group"),
]);

/// Register all compat contract/order classes on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Contract>()?;
    m.add_class::<Order>()?;
    m.add_class::<TagValue>()?;
    m.add_class::<ComboLeg>()?;
    m.add_class::<OrderComboLeg>()?;
    m.add_class::<OrderState>()?;
    m.add_class::<OrderAllocation>()?;
    m.add_class::<PriceCondition>()?;
    m.add_class::<TimeCondition>()?;
    m.add_class::<MarginCondition>()?;
    m.add_class::<ExecutionCondition>()?;
    m.add_class::<VolumeCondition>()?;
    m.add_class::<PercentChangeCondition>()?;
    m.add_class::<BarData>()?;
    m.add_class::<ContractDetails>()?;
    m.add_class::<IneligibilityReasonPy>()?;
    m.add_class::<ContractDescription>()?;
    m.add_class::<CommissionAndFeesReport>()?;
    m.add_class::<Execution>()?;
    m.add_class::<SmartComponentPy>()?;
    m.add_class::<NewsProviderPy>()?;
    m.add_class::<SoftDollarTierPy>()?;
    m.add_class::<DepthMktDataDescriptionPy>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The classes' defaults hold Python objects (None, a new list): the
    // tests need an interpreter.
    fn python() {
        Python::initialize();
    }

    // The official API's defaults of Contract() (ibapi 10.46): empty
    // texts, the strike unset; tests/python/test_compat_classes.py checks
    // them from Python.
    #[test]
    fn contract_default_values() {
        python();
        let c = Contract::default();
        assert_eq!(c.con_id, 0);
        assert_eq!(c.symbol, "");
        assert_eq!(c.sec_type, "");
        assert_eq!(c.exchange, "");
        assert_eq!(c.currency, "");
        assert_eq!(c.strike, f64::MAX);
        // The Rust API's unset strike is 0.
        assert_eq!(c.to_api().strike, 0.0);
    }

    // The official API's defaults of Order() (ibapi 10.46), and the Rust
    // API's are the same.
    #[test]
    fn order_default_values() {
        python();
        let o = Order::default();
        assert_eq!(o.order_id, 0);
        assert_eq!(o.action, "");
        assert_eq!(o.total_quantity, f64::MAX);
        assert_eq!(o.order_type, "");
        assert_eq!((o.lmt_price, o.aux_price, o.trailing_percent, o.cash_qty), (f64::MAX, f64::MAX, f64::MAX, f64::MAX));
        assert_eq!((o.min_qty, o.volatility_type, o.reference_price_type), (i32::MAX, i32::MAX, i32::MAX));
        assert_eq!(o.tif, "");
        assert!(o.transmit);
        assert!(!o.what_if);
        assert!(!o.outside_rth);
        assert!(!o.dont_use_auto_price_for_hedge);
        let api = o.to_api();
        let d = crate::api::types::Order::default();
        assert_eq!((api.total_quantity, api.lmt_price, api.aux_price, api.tif.as_str()), (d.total_quantity, d.lmt_price, d.aux_price, d.tif.as_str()));
        assert_eq!((api.min_qty, api.trailing_percent, api.cash_qty, api.trigger_price), (d.min_qty, d.trailing_percent, d.cash_qty, d.trigger_price));
        // Nothing set, nothing extended.
        assert!(!api.has_extended_attrs());
        assert_eq!((api.attrs().min_qty, api.attrs().cash_qty), (0, 0));
    }

    #[test]
    fn order_side_parsing() {
        python();
        let mut o = Order::default();
        o.action = "BUY".into();
        assert_eq!(o.side().unwrap(), Side::Buy);
        o.action = "SELL".into();
        assert_eq!(o.side().unwrap(), Side::Sell);
        o.action = "SSHORT".into();
        assert_eq!(o.side().unwrap(), Side::ShortSell);
    }

    #[test]
    fn order_tif_byte_mapping() {
        python();
        let mut o = Order::default();
        o.tif = "DAY".into();
        assert_eq!(o.tif_byte(), b'0');
        o.tif = "GTC".into();
        assert_eq!(o.tif_byte(), b'1');
        o.tif = "IOC".into();
        assert_eq!(o.tif_byte(), b'3');
        o.tif = "FOK".into();
        assert_eq!(o.tif_byte(), b'4');
    }

    #[test]
    fn order_has_extended_attrs() {
        python();
        let o = Order::default();
        assert!(!o.has_extended_attrs());

        let mut o2 = Order::default();
        o2.hidden = true;
        assert!(o2.has_extended_attrs());
    }

    #[test]
    fn order_attrs_conversion() {
        python();
        let mut o = Order::default();
        o.display_size = 50;
        o.hidden = true;
        o.discretionary_amt = 0.05;
        let attrs = o.attrs();
        assert_eq!(attrs.display_size, 50);
        assert!(attrs.hidden);
        assert_eq!(attrs.discretionary_amt, (0.05 * PRICE_SCALE_F) as Price);
    }

    // ibx#395: the OCA type, trail stop price and limit price offset reach
    // the engine order; unset they stay unset.
    #[test]
    fn order_to_api_carries_oca_type_trail_stop_and_offset() {
        python();
        let d = Order::default().to_api();
        assert_eq!(d.oca_type, 0);
        assert_eq!(d.trail_stop_price, f64::MAX);
        assert_eq!(d.lmt_price_offset, f64::MAX);

        let mut o = Order::default();
        o.action = "SELL".into();
        o.total_quantity = 10.0;
        o.order_type = "TRAIL LIMIT".into();
        o.aux_price = 1.0;
        o.oca_group = "g1".into();
        o.oca_type = 2;
        o.trail_stop_price = 99.5;
        o.lmt_price_offset = 0.25;
        let api = o.to_api();
        assert_eq!(api.oca_type, 2);
        assert_eq!(api.trail_stop_price, 99.5);
        assert_eq!(api.lmt_price_offset, 0.25);
        assert_eq!(api.attrs().oca_type, 2);

        match crate::client_core::ClientCore::build_order_request(&api, 7, 0).unwrap() {
            ControlCommand::Order(OrderRequest::SubmitTrailingStopLimit { lmt_offset, lmt_price, trail_stop_price, .. })
            | ControlCommand::Order(OrderRequest::SubmitEx {
                kind: OrderKind::TrailingStopLimit { lmt_offset, lmt_price, trail_stop_price, .. }, ..
            }) => {
                assert_eq!(lmt_offset, (0.25 * PRICE_SCALE_F) as Price);
                assert_eq!(lmt_price, None);
                assert_eq!(trail_stop_price, (99.5 * PRICE_SCALE_F) as Price);
            }
            other => panic!("expected a trailing stop limit, got {other:?}"),
        }
    }

    #[test]
    fn tag_value_fields() {
        let tv = TagValue { tag: "maxPctVol".into(), value: "0.1".into() };
        assert_eq!(tv.tag, "maxPctVol");
        assert_eq!(tv.value, "0.1");
    }

    #[test]
    fn price_condition_to_internal() {
        let pc = PriceCondition {
            con_id: Some(265598),
            exchange: Some("SMART".into()),
            price: Some(200.0),
            is_more: Some(true),
            trigger_method: Some(1),
        };
        match pc.to_internal() {
            OrderCondition::Price { con_id, price, is_more, trigger_method, .. } => {
                assert_eq!(con_id, 265598);
                assert_eq!(price, (200.0 * PRICE_SCALE_F) as Price);
                assert!(is_more);
                assert_eq!(trigger_method, 1);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn time_condition_to_internal() {
        let tc = TimeCondition { time: Some("20260311-09:30:00".into()), is_more: Some(true) };
        match tc.to_internal() {
            OrderCondition::Time { time, is_more } => {
                assert_eq!(time, "20260311-09:30:00");
                assert!(is_more);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn volume_condition_to_internal() {
        let vc = VolumeCondition {
            con_id: Some(265598),
            exchange: Some("SMART".into()),
            volume: Some(1_000_000),
            is_more: Some(true),
        };
        match vc.to_internal() {
            OrderCondition::Volume { con_id, volume, is_more, .. } => {
                assert_eq!(con_id, 265598);
                assert_eq!(volume, 1_000_000);
                assert!(is_more);
            }
            _ => panic!("wrong variant"),
        }
    }
}
