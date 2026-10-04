//! ibapi-compatible tick type constants and TickAttrib classes.

use pyo3::prelude::*;

use super::values::{decimal_from_py, decimal_to_py, official_names};

// ── Tick type constants matching ibapi's TickTypeEnum ──

pub const TICK_BID_SIZE: i32 = 0;
pub const TICK_BID: i32 = 1;
pub const TICK_ASK: i32 = 2;
pub const TICK_ASK_SIZE: i32 = 3;
pub const TICK_LAST: i32 = 4;
pub const TICK_LAST_SIZE: i32 = 5;
pub const TICK_HIGH: i32 = 6;
pub const TICK_LOW: i32 = 7;
pub const TICK_VOLUME: i32 = 8;
pub const TICK_CLOSE: i32 = 9;
pub const TICK_OPEN: i32 = 14;
pub const TICK_BID_EXCHANGE: i32 = 32;
pub const TICK_ASK_EXCHANGE: i32 = 33;
pub const TICK_LAST_TIMESTAMP: i32 = 45;
pub const TICK_HALTED: i32 = 49;
pub const TICK_LAST_EXCHANGE: i32 = 84;

/// ibapi-compatible TickAttrib for tickPrice callbacks.
#[pyclass(from_py_object)]
#[derive(Clone, Default)]
pub struct TickAttrib {
    #[pyo3(get, set)]
    pub can_auto_execute: bool,
    #[pyo3(get, set)]
    pub past_limit: bool,
    #[pyo3(get, set)]
    pub pre_open: bool,
}

#[pymethods]
impl TickAttrib {
    #[new]
    #[pyo3(signature = (can_auto_execute=false, past_limit=false, pre_open=false))]
    fn new(can_auto_execute: bool, past_limit: bool, pre_open: bool) -> Self {
        Self { can_auto_execute, past_limit, pre_open }
    }

    fn __repr__(&self) -> String {
        format!("TickAttrib(canAutoExecute={}, pastLimit={}, preOpen={})",
            self.can_auto_execute, self.past_limit, self.pre_open)
    }

    // ibapi camelCase aliases (ibx#487: the official client library's names)
    #[getter(canAutoExecute)]
    fn get_can_auto_execute_alias(&self) -> bool { self.can_auto_execute }
    #[setter(canAutoExecute)]
    fn set_can_auto_execute_alias(&mut self, v: bool) { self.can_auto_execute = v; }
    #[getter(pastLimit)]
    fn get_past_limit_alias(&self) -> bool { self.past_limit }
    #[setter(pastLimit)]
    fn set_past_limit_alias(&mut self, v: bool) { self.past_limit = v; }
    #[getter(preOpen)]
    fn get_pre_open_alias(&self) -> bool { self.pre_open }
    #[setter(preOpen)]
    fn set_pre_open_alias(&mut self, v: bool) { self.pre_open = v; }
}

/// ibapi-compatible TickAttribLast for tick-by-tick last/allLast callbacks.
#[pyclass(from_py_object)]
#[derive(Clone, Default)]
pub struct TickAttribLast {
    #[pyo3(get, set)]
    pub past_limit: bool,
    #[pyo3(get, set)]
    pub unreported: bool,
}

#[pymethods]
impl TickAttribLast {
    #[new]
    #[pyo3(signature = (past_limit=false, unreported=false))]
    fn new(past_limit: bool, unreported: bool) -> Self {
        Self { past_limit, unreported }
    }

    fn __repr__(&self) -> String {
        format!("TickAttribLast(pastLimit={}, unreported={})", self.past_limit, self.unreported)
    }
}

official_names!(TickAttribLast, [("pastLimit", "past_limit")]);

/// ibapi-compatible TickAttribBidAsk for tick-by-tick bid/ask callbacks.
#[pyclass(from_py_object)]
#[derive(Clone, Default)]
pub struct TickAttribBidAsk {
    #[pyo3(get, set)]
    pub bid_past_low: bool,
    #[pyo3(get, set)]
    pub ask_past_high: bool,
}

#[pymethods]
impl TickAttribBidAsk {
    #[new]
    #[pyo3(signature = (bid_past_low=false, ask_past_high=false))]
    fn new(bid_past_low: bool, ask_past_high: bool) -> Self {
        Self { bid_past_low, ask_past_high }
    }

    fn __repr__(&self) -> String {
        format!("TickAttribBidAsk(bidPastLow={}, askPastHigh={})", self.bid_past_low, self.ask_past_high)
    }
}

official_names!(TickAttribBidAsk, [("bidPastLow", "bid_past_low"), ("askPastHigh", "ask_past_high")]);

/// ibapi-compatible HistoricalTick (a midpoint) for historicalTicks
/// (ibx#432): time in Unix seconds. The size is the official API's Decimal
/// (`decimal.Decimal`, unset `UNSET_DECIMAL`; `f64::MAX` here).
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct HistoricalTick {
    #[pyo3(get, set)]
    pub time: i64,
    #[pyo3(get, set)]
    pub price: f64,
    pub size: f64,
}

#[pymethods]
impl HistoricalTick {
    #[new]
    #[pyo3(signature = (time=0, price=0.0, size=None))]
    fn new(time: i64, price: f64, size: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        Ok(Self { time, price, size: size.map(decimal_from_py).transpose()?.unwrap_or(f64::MAX) })
    }

    #[getter(size)]
    fn get_size(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.size) }
    #[setter(size)]
    fn set_size(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.size = decimal_from_py(v)?; Ok(()) }

    fn __repr__(&self) -> String {
        format!("HistoricalTick(time={}, price={}, size={})", self.time, self.price, self.size)
    }
}

/// ibapi-compatible HistoricalTickLast for historicalTicksLast (ibx#432).
/// The size is the official API's Decimal, as `HistoricalTick`'s.
#[pyclass]
pub struct HistoricalTickLast {
    #[pyo3(get, set)]
    pub time: i64,
    #[pyo3(get, set)]
    pub tick_attrib_last: Py<TickAttribLast>,
    #[pyo3(get, set)]
    pub price: f64,
    pub size: f64,
    #[pyo3(get, set)]
    pub exchange: String,
    #[pyo3(get, set)]
    pub special_conditions: String,
}

#[pymethods]
impl HistoricalTickLast {
    /// The official API's defaults.
    #[new]
    fn new(py: Python<'_>) -> PyResult<Self> {
        Ok(Self {
            time: 0, tick_attrib_last: Py::new(py, TickAttribLast::default())?, price: 0.0, size: f64::MAX,
            exchange: String::new(), special_conditions: String::new(),
        })
    }

    #[getter(size)]
    fn get_size(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.size) }
    #[setter(size)]
    fn set_size(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.size = decimal_from_py(v)?; Ok(()) }

    fn __repr__(&self, py: Python<'_>) -> String {
        let a = self.tick_attrib_last.borrow(py);
        format!("HistoricalTickLast(time={}, pastLimit={}, unreported={}, price={}, size={}, exchange={}, specialConditions={:?})",
            self.time, a.past_limit, a.unreported, self.price, self.size, self.exchange, self.special_conditions)
    }
}

official_names!(HistoricalTickLast, [("tickAttribLast", "tick_attrib_last"), ("specialConditions", "special_conditions")]);

/// ibapi-compatible HistoricalTickBidAsk for historicalTicksBidAsk
/// (ibx#432). The sizes are the official API's Decimals, as
/// `HistoricalTick`'s.
#[pyclass]
pub struct HistoricalTickBidAsk {
    #[pyo3(get, set)]
    pub time: i64,
    #[pyo3(get, set)]
    pub tick_attrib_bid_ask: Py<TickAttribBidAsk>,
    #[pyo3(get, set)]
    pub price_bid: f64,
    #[pyo3(get, set)]
    pub price_ask: f64,
    pub size_bid: f64,
    pub size_ask: f64,
}

#[pymethods]
impl HistoricalTickBidAsk {
    /// The official API's defaults.
    #[new]
    fn new(py: Python<'_>) -> PyResult<Self> {
        Ok(Self {
            time: 0, tick_attrib_bid_ask: Py::new(py, TickAttribBidAsk::default())?, price_bid: 0.0, price_ask: 0.0,
            size_bid: f64::MAX, size_ask: f64::MAX,
        })
    }

    #[getter(size_bid)]
    fn get_size_bid(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.size_bid) }
    #[setter(size_bid)]
    fn set_size_bid(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.size_bid = decimal_from_py(v)?; Ok(()) }
    #[getter(size_ask)]
    fn get_size_ask(&self, py: Python<'_>) -> PyResult<Py<PyAny>> { decimal_to_py(py, self.size_ask) }
    #[setter(size_ask)]
    fn set_size_ask(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> { self.size_ask = decimal_from_py(v)?; Ok(()) }

    fn __repr__(&self, py: Python<'_>) -> String {
        let a = self.tick_attrib_bid_ask.borrow(py);
        format!("HistoricalTickBidAsk(time={}, bidPastLow={}, askPastHigh={}, priceBid={}, priceAsk={}, sizeBid={}, sizeAsk={})",
            self.time, a.bid_past_low, a.ask_past_high, self.price_bid, self.price_ask, self.size_bid, self.size_ask)
    }
}

official_names!(HistoricalTickBidAsk, [
    ("tickAttribBidAsk", "tick_attrib_bid_ask"), ("priceBid", "price_bid"), ("priceAsk", "price_ask"),
    ("sizeBid", "size_bid"), ("sizeAsk", "size_ask"),
]);

/// Module-level TickTypeEnum class for accessing tick type constants.
#[pyclass]
pub struct TickTypeEnum;

#[pymethods]
impl TickTypeEnum {
    #[classattr]
    const BID_SIZE: i32 = TICK_BID_SIZE;
    #[classattr]
    const BID: i32 = TICK_BID;
    #[classattr]
    const ASK: i32 = TICK_ASK;
    #[classattr]
    const ASK_SIZE: i32 = TICK_ASK_SIZE;
    #[classattr]
    const LAST: i32 = TICK_LAST;
    #[classattr]
    const LAST_SIZE: i32 = TICK_LAST_SIZE;
    #[classattr]
    const HIGH: i32 = TICK_HIGH;
    #[classattr]
    const LOW: i32 = TICK_LOW;
    #[classattr]
    const VOLUME: i32 = TICK_VOLUME;
    #[classattr]
    const CLOSE: i32 = TICK_CLOSE;
    #[classattr]
    const OPEN: i32 = TICK_OPEN;
    #[classattr]
    const LAST_TIMESTAMP: i32 = TICK_LAST_TIMESTAMP;
    #[classattr]
    const HALTED: i32 = TICK_HALTED;
    #[classattr]
    const BID_EXCHANGE: i32 = TICK_BID_EXCHANGE;
    #[classattr]
    const ASK_EXCHANGE: i32 = TICK_ASK_EXCHANGE;
    #[classattr]
    const LAST_EXCHANGE: i32 = TICK_LAST_EXCHANGE;
}

/// Register tick type classes and constants on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<TickTypeEnum>()?;
    m.add_class::<TickAttrib>()?;
    m.add_class::<TickAttribLast>()?;
    m.add_class::<TickAttribBidAsk>()?;
    m.add_class::<HistoricalTick>()?;
    m.add_class::<HistoricalTickLast>()?;
    m.add_class::<HistoricalTickBidAsk>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_type_constants_match_ibapi() {
        assert_eq!(TICK_BID_SIZE, 0);
        assert_eq!(TICK_BID, 1);
        assert_eq!(TICK_ASK, 2);
        assert_eq!(TICK_ASK_SIZE, 3);
        assert_eq!(TICK_LAST, 4);
        assert_eq!(TICK_LAST_SIZE, 5);
        assert_eq!(TICK_HIGH, 6);
        assert_eq!(TICK_LOW, 7);
        assert_eq!(TICK_VOLUME, 8);
        assert_eq!(TICK_CLOSE, 9);
        assert_eq!(TICK_OPEN, 14);
        assert_eq!(TICK_LAST_TIMESTAMP, 45);
        assert_eq!(TICK_HALTED, 49);
    }

    #[test]
    fn tick_attrib_defaults() {
        let ta = TickAttrib::default();
        assert!(!ta.can_auto_execute);
        assert!(!ta.past_limit);
        assert!(!ta.pre_open);
    }

    #[test]
    fn tick_attrib_last_defaults() {
        let ta = TickAttribLast::default();
        assert!(!ta.past_limit);
        assert!(!ta.unreported);
    }

    #[test]
    fn tick_attrib_bid_ask_defaults() {
        let ta = TickAttribBidAsk::default();
        assert!(!ta.bid_past_low);
        assert!(!ta.ask_past_high);
    }
}
