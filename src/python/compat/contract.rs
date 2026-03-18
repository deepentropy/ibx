//! ibapi-compatible Contract, Order, TagValue, and condition classes.

use pyo3::prelude::*;
use pyo3::exceptions::PyRuntimeError;

use crate::types::*;
use super::super::types::PRICE_SCALE_F;

// ── Contract ──

/// ibapi-compatible Contract class.
#[pyclass]
#[derive(Clone, Default)]
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
}

#[pymethods]
impl Contract {
    #[new]
    #[pyo3(signature = (con_id=0, symbol="".to_string(), sec_type="STK".to_string(), exchange="SMART".to_string(), currency="USD".to_string(), last_trade_date_or_contract_month="".to_string(), strike=0.0, right="".to_string(), multiplier="".to_string(), local_symbol="".to_string(), primary_exchange="".to_string(), trading_class="".to_string()))]
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
        }
    }

    fn __repr__(&self) -> String {
        format!("Contract(conId={}, symbol='{}', secType='{}', exchange='{}')",
            self.con_id, self.symbol, self.sec_type, self.exchange)
    }
}

// ── Order ──

/// ibapi-compatible Order class.
#[pyclass]
pub struct Order {
    #[pyo3(get, set)]
    pub order_id: i64,
    #[pyo3(get, set)]
    pub action: String,
    #[pyo3(get, set)]
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
    #[pyo3(get, set)]
    pub algo_params: Vec<TagValue>,
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
    pub conditions: Vec<PyObject>,
    #[pyo3(get, set)]
    pub conditions_ignore_rth: bool,
    #[pyo3(get, set)]
    pub conditions_cancel_order: bool,
}

impl Default for Order {
    fn default() -> Self {
        Self {
            order_id: 0,
            action: String::new(),
            total_quantity: 0.0,
            order_type: String::new(),
            lmt_price: 0.0,
            aux_price: 0.0,
            tif: "DAY".into(),
            outside_rth: false,
            display_size: 0,
            min_qty: 0,
            hidden: false,
            good_after_time: String::new(),
            good_till_date: String::new(),
            oca_group: String::new(),
            trailing_percent: 0.0,
            algo_strategy: String::new(),
            algo_params: Vec::new(),
            what_if: false,
            cash_qty: 0.0,
            parent_id: 0,
            transmit: true,
            discretionary_amt: 0.0,
            sweep_to_fill: false,
            all_or_none: false,
            trigger_method: 0,
            adjusted_order_type: String::new(),
            trigger_price: 0.0,
            adjusted_stop_price: 0.0,
            adjusted_stop_limit_price: 0.0,
            conditions: Vec::new(),
            conditions_ignore_rth: false,
            conditions_cancel_order: false,
        }
    }
}

#[pymethods]
impl Order {
    #[new]
    #[pyo3(signature = (
        order_id=0, action="".to_string(), total_quantity=0.0, order_type="".to_string(),
        lmt_price=0.0, aux_price=0.0, tif="DAY".to_string(), outside_rth=false,
        display_size=0, min_qty=0, hidden=false, good_after_time="".to_string(),
        good_till_date="".to_string(), oca_group="".to_string(), trailing_percent=0.0,
        algo_strategy="".to_string(), what_if=false, cash_qty=0.0, parent_id=0,
        transmit=true
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        order_id: i64,
        action: String,
        total_quantity: f64,
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
    ) -> Self {
        Self {
            order_id,
            action,
            total_quantity,
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
            algo_params: Vec::new(),
            what_if,
            cash_qty,
            parent_id,
            transmit,
            ..Default::default()
        }
    }

    fn __repr__(&self) -> String {
        format!("Order(orderId={}, action='{}', totalQuantity={}, orderType='{}', lmtPrice={}, auxPrice={})",
            self.order_id, self.action, self.total_quantity, self.order_type, self.lmt_price, self.aux_price)
    }

    // ibapi camelCase aliases
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
    #[getter(totalQuantity)]
    fn get_total_quantity_alias(&self) -> f64 { self.total_quantity }
    #[setter(totalQuantity)]
    fn set_total_quantity_alias(&mut self, v: f64) { self.total_quantity = v; }
    #[getter(orderType)]
    fn get_order_type_alias(&self) -> String { self.order_type.clone() }
    #[setter(orderType)]
    fn set_order_type_alias(&mut self, v: String) { self.order_type = v; }
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
    /// Convert to Rust API Contract.
    pub fn to_api(&self) -> crate::api::types::Contract {
        crate::api::types::Contract {
            con_id: self.con_id,
            symbol: self.symbol.clone(),
            sec_type: self.sec_type.clone(),
            exchange: self.exchange.clone(),
            currency: self.currency.clone(),
            last_trade_date_or_contract_month: self.last_trade_date_or_contract_month.clone(),
            strike: self.strike,
            right: self.right.clone(),
            multiplier: self.multiplier.clone(),
            local_symbol: self.local_symbol.clone(),
            primary_exchange: self.primary_exchange.clone(),
            trading_class: self.trading_class.clone(),
        }
    }
}

impl Order {
    /// Convert PyObject conditions to internal OrderCondition list.
    pub fn convert_conditions(&self, py: Python<'_>) -> Vec<OrderCondition> {
        self.conditions.iter().filter_map(|obj| {
            let any = obj.bind(py);
            if let Ok(c) = any.downcast::<PriceCondition>() { return Some(c.borrow().to_internal()); }
            if let Ok(c) = any.downcast::<TimeCondition>() { return Some(c.borrow().to_internal()); }
            if let Ok(c) = any.downcast::<MarginCondition>() { return Some(c.borrow().to_internal()); }
            if let Ok(c) = any.downcast::<ExecutionCondition>() { return Some(c.borrow().to_internal()); }
            if let Ok(c) = any.downcast::<VolumeCondition>() { return Some(c.borrow().to_internal()); }
            if let Ok(c) = any.downcast::<PercentChangeCondition>() { return Some(c.borrow().to_internal()); }
            log::warn!("Unknown order condition type, skipping");
            None
        }).collect()
    }

    /// Convert to Rust API Order.
    pub fn to_api(&self) -> crate::api::types::Order {
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
            algo_params: self.algo_params.iter().map(|tv| crate::api::types::TagValue {
                tag: tv.tag.clone(),
                value: tv.value.clone(),
            }).collect(),
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
            conditions: Vec::new(), // Use convert_conditions(py) + to_api() at call sites that need conditions
            conditions_ignore_rth: self.conditions_ignore_rth,
            conditions_cancel_order: self.conditions_cancel_order,
        }
    }
}

// ── TagValue ──

/// ibapi-compatible TagValue for algo parameters.
#[pyclass]
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
    fn new(tag: String, value: String) -> Self {
        Self { tag, value }
    }

    fn __repr__(&self) -> String {
        format!("TagValue(tag='{}', value='{}')", self.tag, self.value)
    }
}

// ── OrderState (for what-if responses) ──

/// ibapi-compatible OrderState class (used in openOrder callback).
#[pyclass]
#[derive(Clone, Default)]
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
    pub init_margin_after: String,
    #[pyo3(get, set)]
    pub maint_margin_after: String,
    #[pyo3(get, set)]
    pub equity_with_loan_after: String,
    #[pyo3(get, set)]
    pub commission: f64,
    #[pyo3(get, set)]
    pub min_commission: f64,
    #[pyo3(get, set)]
    pub max_commission: f64,
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
}

// ── Order Conditions ──

/// Price condition: trigger when an instrument's price crosses a threshold.
#[pyclass]
#[derive(Clone)]
pub struct PriceCondition {
    #[pyo3(get, set)]
    pub con_id: i64,
    #[pyo3(get, set)]
    pub exchange: String,
    #[pyo3(get, set)]
    pub price: f64,
    #[pyo3(get, set)]
    pub is_more: bool,
    #[pyo3(get, set)]
    pub trigger_method: i32,
}

#[pymethods]
impl PriceCondition {
    #[new]
    #[pyo3(signature = (con_id=0, exchange="SMART".to_string(), price=0.0, is_more=true, trigger_method=0))]
    fn new(con_id: i64, exchange: String, price: f64, is_more: bool, trigger_method: i32) -> Self {
        Self { con_id, exchange, price, is_more, trigger_method }
    }

    fn __repr__(&self) -> String {
        let op = if self.is_more { ">" } else { "<" };
        format!("PriceCondition(conId={}, price {} {})", self.con_id, op, self.price)
    }
}

impl PriceCondition {
    pub fn to_internal(&self) -> OrderCondition {
        OrderCondition::Price {
            con_id: self.con_id,
            exchange: self.exchange.clone(),
            price: (self.price * PRICE_SCALE_F) as Price,
            is_more: self.is_more,
            trigger_method: self.trigger_method as u8,
        }
    }
}

/// Time condition: trigger at a specific time.
#[pyclass]
#[derive(Clone)]
pub struct TimeCondition {
    #[pyo3(get, set)]
    pub time: String,
    #[pyo3(get, set)]
    pub is_more: bool,
}

#[pymethods]
impl TimeCondition {
    #[new]
    #[pyo3(signature = (time="".to_string(), is_more=true))]
    fn new(time: String, is_more: bool) -> Self {
        Self { time, is_more }
    }

    fn __repr__(&self) -> String {
        let op = if self.is_more { ">" } else { "<" };
        format!("TimeCondition(time {} '{}')", op, self.time)
    }
}

impl TimeCondition {
    pub fn to_internal(&self) -> OrderCondition {
        OrderCondition::Time { time: self.time.clone(), is_more: self.is_more }
    }
}

/// Margin condition: trigger based on margin cushion percentage.
#[pyclass]
#[derive(Clone)]
pub struct MarginCondition {
    #[pyo3(get, set)]
    pub percent: u32,
    #[pyo3(get, set)]
    pub is_more: bool,
}

#[pymethods]
impl MarginCondition {
    #[new]
    #[pyo3(signature = (percent=0, is_more=true))]
    fn new(percent: u32, is_more: bool) -> Self {
        Self { percent, is_more }
    }

    fn __repr__(&self) -> String {
        format!("MarginCondition({}% {})", self.percent, if self.is_more { "above" } else { "below" })
    }
}

impl MarginCondition {
    pub fn to_internal(&self) -> OrderCondition {
        OrderCondition::Margin { percent: self.percent, is_more: self.is_more }
    }
}

/// Execution condition: trigger on trade execution.
#[pyclass]
#[derive(Clone)]
pub struct ExecutionCondition {
    #[pyo3(get, set)]
    pub symbol: String,
    #[pyo3(get, set)]
    pub exchange: String,
    #[pyo3(get, set)]
    pub sec_type: String,
}

#[pymethods]
impl ExecutionCondition {
    #[new]
    #[pyo3(signature = (symbol="".to_string(), exchange="".to_string(), sec_type="".to_string()))]
    fn new(symbol: String, exchange: String, sec_type: String) -> Self {
        Self { symbol, exchange, sec_type }
    }

    fn __repr__(&self) -> String {
        format!("ExecutionCondition(symbol='{}', exchange='{}')", self.symbol, self.exchange)
    }
}

impl ExecutionCondition {
    pub fn to_internal(&self) -> OrderCondition {
        OrderCondition::Execution {
            symbol: self.symbol.clone(),
            exchange: self.exchange.clone(),
            sec_type: self.sec_type.clone(),
        }
    }
}

/// Volume condition: trigger when volume exceeds a threshold.
#[pyclass]
#[derive(Clone)]
pub struct VolumeCondition {
    #[pyo3(get, set)]
    pub con_id: i64,
    #[pyo3(get, set)]
    pub exchange: String,
    #[pyo3(get, set)]
    pub volume: i64,
    #[pyo3(get, set)]
    pub is_more: bool,
}

#[pymethods]
impl VolumeCondition {
    #[new]
    #[pyo3(signature = (con_id=0, exchange="SMART".to_string(), volume=0, is_more=true))]
    fn new(con_id: i64, exchange: String, volume: i64, is_more: bool) -> Self {
        Self { con_id, exchange, volume, is_more }
    }

    fn __repr__(&self) -> String {
        let op = if self.is_more { ">" } else { "<" };
        format!("VolumeCondition(conId={}, volume {} {})", self.con_id, op, self.volume)
    }
}

impl VolumeCondition {
    pub fn to_internal(&self) -> OrderCondition {
        OrderCondition::Volume {
            con_id: self.con_id,
            exchange: self.exchange.clone(),
            volume: self.volume,
            is_more: self.is_more,
        }
    }
}

/// Percentage change condition: trigger on % change from close.
#[pyclass]
#[derive(Clone)]
pub struct PercentChangeCondition {
    #[pyo3(get, set)]
    pub con_id: i64,
    #[pyo3(get, set)]
    pub exchange: String,
    #[pyo3(get, set)]
    pub change_percent: f64,
    #[pyo3(get, set)]
    pub is_more: bool,
}

#[pymethods]
impl PercentChangeCondition {
    #[new]
    #[pyo3(signature = (con_id=0, exchange="SMART".to_string(), change_percent=0.0, is_more=true))]
    fn new(con_id: i64, exchange: String, change_percent: f64, is_more: bool) -> Self {
        Self { con_id, exchange, change_percent, is_more }
    }

    fn __repr__(&self) -> String {
        let op = if self.is_more { ">" } else { "<" };
        format!("PercentChangeCondition(conId={}, {}% {})", self.con_id, op, self.change_percent)
    }
}

impl PercentChangeCondition {
    pub fn to_internal(&self) -> OrderCondition {
        OrderCondition::PercentChange {
            con_id: self.con_id,
            exchange: self.exchange.clone(),
            percent: self.change_percent,
            is_more: self.is_more,
        }
    }
}

// ── BarData ──

/// ibapi-compatible BarData class for historical data callbacks.
#[pyclass]
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
    #[pyo3(get, set)]
    pub volume: i64,
    #[pyo3(get, set)]
    pub wap: f64,
    #[pyo3(get, set)]
    pub bar_count: i32,
}

#[pymethods]
impl BarData {
    #[new]
    #[pyo3(signature = (date="".to_string(), open=0.0, high=0.0, low=0.0, close=0.0, volume=0, wap=0.0, bar_count=0))]
    pub fn new(date: String, open: f64, high: f64, low: f64, close: f64, volume: i64, wap: f64, bar_count: i32) -> Self {
        Self { date, open, high, low, close, volume, wap, bar_count }
    }

    fn __repr__(&self) -> String {
        format!("BarData(date='{}', O={}, H={}, L={}, C={}, V={})",
            self.date, self.open, self.high, self.low, self.close, self.volume)
    }
}

// ── ContractDetails ──

/// ibapi-compatible ContractDetails class.
#[pyclass]
#[derive(Clone, Default)]
pub struct ContractDetails {
    #[pyo3(get, set)]
    pub contract: Contract,
    #[pyo3(get, set)]
    pub market_name: String,
    #[pyo3(get, set)]
    pub min_tick: f64,
    #[pyo3(get, set)]
    pub order_types: String,
    #[pyo3(get, set)]
    pub valid_exchanges: String,
    #[pyo3(get, set)]
    pub long_name: String,
    #[pyo3(get, set)]
    pub last_trade_date: String,
    #[pyo3(get, set)]
    pub multiplier: String,
}

#[pymethods]
impl ContractDetails {
    #[new]
    #[pyo3(signature = ())]
    fn py_new() -> Self {
        Self::default()
    }

    fn __repr__(&self) -> String {
        format!("ContractDetails(symbol='{}', longName='{}')",
            self.contract.symbol, self.long_name)
    }
}

impl ContractDetails {
    pub fn from_definition(def: &crate::control::contracts::ContractDefinition) -> Self {
        let mut c = Contract::default();
        c.con_id = def.con_id as i64;
        c.symbol = def.symbol.clone();
        c.sec_type = format!("{:?}", def.sec_type);
        c.exchange = def.exchange.clone();
        c.primary_exchange = def.primary_exchange.clone();
        c.currency = def.currency.clone();
        c.local_symbol = def.local_symbol.clone();
        c.trading_class = def.trading_class.clone();
        c.last_trade_date_or_contract_month = def.last_trade_date.clone();
        c.strike = def.strike;
        c.multiplier = if def.multiplier != 1.0 { format!("{}", def.multiplier) } else { String::new() };

        Self {
            contract: c,
            market_name: String::new(),
            min_tick: def.min_tick,
            order_types: def.order_types.join(","),
            valid_exchanges: def.valid_exchanges.join(","),
            long_name: def.long_name.clone(),
            last_trade_date: def.last_trade_date.clone(),
            multiplier: if def.multiplier != 1.0 { format!("{}", def.multiplier) } else { String::new() },
        }
    }
}

// ── ContractDescription ──

/// ibapi-compatible ContractDescription class for symbol search results.
#[pyclass]
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
}

#[pymethods]
impl ContractDescription {
    #[new]
    #[pyo3(signature = (con_id=0, symbol="".to_string(), sec_type="".to_string(), currency="".to_string(), primary_exchange="".to_string(), derivative_sec_types=Vec::new()))]
    fn new(con_id: i64, symbol: String, sec_type: String, currency: String, primary_exchange: String, derivative_sec_types: Vec<String>) -> Self {
        Self { con_id, symbol, sec_type, currency, primary_exchange, derivative_sec_types }
    }

    fn __repr__(&self) -> String {
        format!("ContractDescription(conId={}, symbol='{}', secType='{}', currency='{}')",
            self.con_id, self.symbol, self.sec_type, self.currency)
    }
}

/// Register all compat contract/order classes on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Contract>()?;
    m.add_class::<Order>()?;
    m.add_class::<TagValue>()?;
    m.add_class::<OrderState>()?;
    m.add_class::<PriceCondition>()?;
    m.add_class::<TimeCondition>()?;
    m.add_class::<MarginCondition>()?;
    m.add_class::<ExecutionCondition>()?;
    m.add_class::<VolumeCondition>()?;
    m.add_class::<PercentChangeCondition>()?;
    m.add_class::<BarData>()?;
    m.add_class::<ContractDetails>()?;
    m.add_class::<ContractDescription>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_default_values() {
        let c = Contract::default();
        assert_eq!(c.con_id, 0);
        assert_eq!(c.symbol, "");
        assert_eq!(c.sec_type, "");
        assert_eq!(c.exchange, "");
        assert_eq!(c.currency, "");
        assert_eq!(c.strike, 0.0);
    }

    #[test]
    fn order_default_values() {
        let o = Order::default();
        assert_eq!(o.order_id, 0);
        assert_eq!(o.action, "");
        assert_eq!(o.total_quantity, 0.0);
        assert_eq!(o.order_type, "");
        assert_eq!(o.tif, "DAY");
        assert!(o.transmit);
        assert!(!o.what_if);
        assert!(!o.outside_rth);
    }

    #[test]
    fn order_side_parsing() {
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
        let o = Order::default();
        assert!(!o.has_extended_attrs());

        let mut o2 = Order::default();
        o2.hidden = true;
        assert!(o2.has_extended_attrs());
    }

    #[test]
    fn order_attrs_conversion() {
        let mut o = Order::default();
        o.display_size = 50;
        o.hidden = true;
        o.discretionary_amt = 0.05;
        let attrs = o.attrs();
        assert_eq!(attrs.display_size, 50);
        assert!(attrs.hidden);
        assert_eq!(attrs.discretionary_amt, (0.05 * PRICE_SCALE_F) as Price);
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
            con_id: 265598,
            exchange: "SMART".into(),
            price: 200.0,
            is_more: true,
            trigger_method: 1,
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
        let tc = TimeCondition { time: "20260311-09:30:00".into(), is_more: true };
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
            con_id: 265598,
            exchange: "SMART".into(),
            volume: 1_000_000,
            is_more: true,
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
