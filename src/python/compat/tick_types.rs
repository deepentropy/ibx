//! ibapi-compatible tick type constants and TickAttrib classes.

use pyo3::prelude::*;

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
pub const TICK_LAST_TIMESTAMP: i32 = 45;
pub const TICK_HALTED: i32 = 49;

/// ibapi-compatible TickAttrib for tickPrice callbacks.
#[pyclass]
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
}

/// ibapi-compatible TickAttribLast for tick-by-tick last/allLast callbacks.
#[pyclass]
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

/// ibapi-compatible TickAttribBidAsk for tick-by-tick bid/ask callbacks.
#[pyclass]
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
}

/// Register tick type classes and constants on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<TickTypeEnum>()?;
    m.add_class::<TickAttrib>()?;
    m.add_class::<TickAttribLast>()?;
    m.add_class::<TickAttribBidAsk>()?;
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
