//! ibapi-compatible tick type constants and TickAttrib classes.

use pyo3::prelude::*;

use super::{camel_aliases_copy};

/// Every tick this venue numbers, by the name it goes by, in the order that
/// numbers them.
///
/// The position is the number. The reference client builds its own table by
/// enumerating exactly this list, so the index of a name here is the tick type
/// a callback is handed, and a program reads a tick back by looking the number
/// up in it. Held whole rather than as the handful this client names in Rust:
/// short, every name past the end is absent and every number past it reads as
/// nothing at all.
pub static TICK_TYPE_NAMES: [&str; 112] = [
    "BID_SIZE", "BID", "ASK", "ASK_SIZE", "LAST", "LAST_SIZE", "HIGH", "LOW", "VOLUME", "CLOSE",
    "BID_OPTION_COMPUTATION", "ASK_OPTION_COMPUTATION", "LAST_OPTION_COMPUTATION",
    "MODEL_OPTION", "OPEN", "LOW_13_WEEK", "HIGH_13_WEEK", "LOW_26_WEEK", "HIGH_26_WEEK",
    "LOW_52_WEEK", "HIGH_52_WEEK", "AVG_VOLUME", "OPEN_INTEREST", "OPTION_HISTORICAL_VOL",
    "OPTION_IMPLIED_VOL", "OPTION_BID_EXCH", "OPTION_ASK_EXCH", "OPTION_CALL_OPEN_INTEREST",
    "OPTION_PUT_OPEN_INTEREST", "OPTION_CALL_VOLUME", "OPTION_PUT_VOLUME",
    "INDEX_FUTURE_PREMIUM", "BID_EXCH", "ASK_EXCH", "AUCTION_VOLUME", "AUCTION_PRICE",
    "AUCTION_IMBALANCE", "MARK_PRICE", "BID_EFP_COMPUTATION", "ASK_EFP_COMPUTATION",
    "LAST_EFP_COMPUTATION", "OPEN_EFP_COMPUTATION", "HIGH_EFP_COMPUTATION",
    "LOW_EFP_COMPUTATION", "CLOSE_EFP_COMPUTATION", "LAST_TIMESTAMP", "SHORTABLE",
    "FUNDAMENTAL_RATIOS",
    "RT_VOLUME", "HALTED", "BID_YIELD", "ASK_YIELD", "LAST_YIELD", "CUST_OPTION_COMPUTATION",
    "TRADE_COUNT", "TRADE_RATE", "VOLUME_RATE", "LAST_RTH_TRADE", "RT_HISTORICAL_VOL",
    "IB_DIVIDENDS", "BOND_FACTOR_MULTIPLIER", "REGULATORY_IMBALANCE", "NEWS_TICK",
    "SHORT_TERM_VOLUME_3_MIN", "SHORT_TERM_VOLUME_5_MIN", "SHORT_TERM_VOLUME_10_MIN",
    "DELAYED_BID", "DELAYED_ASK", "DELAYED_LAST", "DELAYED_BID_SIZE", "DELAYED_ASK_SIZE",
    "DELAYED_LAST_SIZE", "DELAYED_HIGH", "DELAYED_LOW", "DELAYED_VOLUME", "DELAYED_CLOSE",
    "DELAYED_OPEN", "RT_TRD_VOLUME", "CREDITMAN_MARK_PRICE", "CREDITMAN_SLOW_MARK_PRICE",
    "DELAYED_BID_OPTION", "DELAYED_ASK_OPTION", "DELAYED_LAST_OPTION", "DELAYED_MODEL_OPTION",
    "LAST_EXCH", "LAST_REG_TIME", "FUTURES_OPEN_INTEREST", "AVG_OPT_VOLUME",
    "DELAYED_LAST_TIMESTAMP", "SHORTABLE_SHARES", "DELAYED_HALTED", "REUTERS_2_MUTUAL_FUNDS",
    "ETF_NAV_CLOSE", "ETF_NAV_PRIOR_CLOSE", "ETF_NAV_BID", "ETF_NAV_ASK", "ETF_NAV_LAST",
    "ETF_FROZEN_NAV_LAST", "ETF_NAV_HIGH", "ETF_NAV_LOW", "SOCIAL_MARKET_ANALYTICS",
    "ESTIMATED_IPO_MIDPOINT", "FINAL_IPO_LAST", "DELAYED_YIELD_BID", "DELAYED_YIELD_ASK",
    "ODD_LOT_BID", "ODD_LOT_ASK", "ODD_LOT_BID_SIZE", "ODD_LOT_ASK_SIZE", "ODD_LOT_BID_EXCH",
    "ODD_LOT_ASK_EXCH", "NOT_SET",
];

// ── The one tick this module names in Rust ──
//
// The rest were a second copy of the numbers, written out to hang the class
// attributes off. Named from the table above now, so there is nothing here
// for the two copies to disagree about.

pub use crate::client_core::TICK_LAST_TIMESTAMP;

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

/// Module-level TickTypeEnum class for accessing tick type constants.
#[pyclass]
pub struct TickTypeEnum;

#[pymethods]
impl TickTypeEnum {
    /// The name a tick number goes by, or `NOTFOUND` where it names none.
    ///
    /// The spelling is the reference client's, which is how a program written
    /// against it reads a tick back: it keys what it collected on this.
    #[staticmethod]
    #[pyo3(name = "toStr")]
    fn to_str(idx: i64) -> &'static str {
        usize::try_from(idx)
            .ok()
            .and_then(|i| TICK_TYPE_NAMES.get(i))
            .copied()
            .unwrap_or("NOTFOUND")
    }

    /// Every tick number and the name it goes by.
    #[classattr]
    fn idx2name() -> std::collections::HashMap<i64, &'static str> {
        TICK_TYPE_NAMES.iter().enumerate().map(|(i, n)| (i as i64, *n)).collect()
    }
}

/// Register tick type classes and constants on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<TickTypeEnum>()?;
    // Named from the one table rather than written out here: sixteen were
    // written out, and a program asking for any of the other ninety-six got
    // an AttributeError partway through a callback.
    let tick_types = m.getattr("TickTypeEnum")?;
    for (number, name) in TICK_TYPE_NAMES.iter().enumerate() {
        tick_types.setattr(*name, number as i64)?;
    }
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

    /// The numbers this client names in Rust are the places those names hold
    /// in the table the class is built from.
    ///
    /// Two lists of the same numbers is the drift this pins: the table is what
    /// a program reads a tick back by, `client_core` is what the decode path
    /// compares against, and a tick renumbered in one and not the other files a
    /// quote under a name that is not its own.
    #[test]
    fn the_table_and_the_names_this_client_uses_agree() {
        use crate::client_core as core;
        for (number, name) in [
            (core::TICK_BID_SIZE, "BID_SIZE"),
            (core::TICK_BID, "BID"),
            (core::TICK_ASK, "ASK"),
            (core::TICK_ASK_SIZE, "ASK_SIZE"),
            (core::TICK_LAST, "LAST"),
            (core::TICK_LAST_SIZE, "LAST_SIZE"),
            (core::TICK_HIGH, "HIGH"),
            (core::TICK_LOW, "LOW"),
            (core::TICK_VOLUME, "VOLUME"),
            (core::TICK_CLOSE, "CLOSE"),
            (core::TICK_OPEN, "OPEN"),
            (core::TICK_LAST_TIMESTAMP, "LAST_TIMESTAMP"),
            (core::TICK_HALTED, "HALTED"),
            // Named in full in Rust; the table holds the spelling a program
            // written against the reference client asks for.
            (core::TICK_BID_EXCHANGE, "BID_EXCH"),
            (core::TICK_ASK_EXCHANGE, "ASK_EXCH"),
            (core::TICK_LAST_EXCHANGE, "LAST_EXCH"),
        ] {
            assert_eq!(
                TICK_TYPE_NAMES[number as usize], name,
                "tick {number} is {} in the table",
                TICK_TYPE_NAMES[number as usize],
            );
        }
    }

    /// Every tick the venue numbers is in the table, once.
    #[test]
    fn the_table_names_each_tick_once() {
        assert_eq!(TICK_TYPE_NAMES.len(), 112);
        let mut seen: Vec<&str> = TICK_TYPE_NAMES.to_vec();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before, "a name is in the table twice");
        assert_eq!(TickTypeEnum::to_str(1), "BID");
        assert_eq!(TickTypeEnum::to_str(111), "NOT_SET");
        assert_eq!(TickTypeEnum::to_str(112), "NOTFOUND");
        assert_eq!(TickTypeEnum::to_str(-1), "NOTFOUND");
    }

    /// The tick between the shortable one and the running volume has a name.
    ///
    /// A program reads a tick back by looking its number up in this table.
    /// Held as a placeholder, the fundamentals figures arrived under a name
    /// saying the number is unused, and a caller had no way to tell them from
    /// a number the venue does not send at all.
    #[test]
    fn the_fundamentals_tick_is_named() {
        assert_eq!(TickTypeEnum::to_str(47), "FUNDAMENTAL_RATIOS");
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

/// One historical tick, as the reference client hands it over.
///
/// A tuple carries the same numbers and answers to none of the names, so a
/// program reading `tick.price` off what it was given finds nothing there.
/// `size` is stated where the venue states one; a midpoint has none.
#[pyclass(from_py_object)]
#[derive(Clone, Default)]
pub struct HistoricalTick {
    #[pyo3(get, set)]
    pub time: i64,
    #[pyo3(get, set)]
    pub price: f64,
    #[pyo3(get, set)]
    pub size: f64,
}

#[pymethods]
impl HistoricalTick {
    #[new]
    #[pyo3(signature = (time=0, price=0.0, size=0.0))]
    fn new(time: i64, price: f64, size: f64) -> Self {
        Self { time, price, size }
    }

    fn __repr__(&self) -> String {
        format!("HistoricalTick(time={}, price={}, size={})", self.time, self.price, self.size)
    }
}

/// One historical trade, with what the venue said about it.
#[pyclass(from_py_object)]
#[derive(Clone, Default)]
pub struct HistoricalTickLast {
    #[pyo3(get, set)]
    pub time: i64,
    #[pyo3(get, set)]
    pub tick_attrib_last: TickAttribLast,
    #[pyo3(get, set)]
    pub price: f64,
    #[pyo3(get, set)]
    pub size: f64,
    #[pyo3(get, set)]
    pub exchange: String,
    #[pyo3(get, set)]
    pub special_conditions: String,
}

#[pymethods]
impl HistoricalTickLast {
    #[new]
    #[pyo3(signature = (time=0, tick_attrib_last=TickAttribLast::default(), price=0.0,
                        size=0.0, exchange=String::new(), special_conditions=String::new()))]
    fn new(
        time: i64, tick_attrib_last: TickAttribLast, price: f64, size: f64,
        exchange: String, special_conditions: String,
    ) -> Self {
        Self { time, tick_attrib_last, price, size, exchange, special_conditions }
    }

    #[getter(tickAttribLast)]
    fn get_tick_attrib_last(&self) -> TickAttribLast { self.tick_attrib_last.clone() }
    #[setter(tickAttribLast)]
    fn set_tick_attrib_last(&mut self, v: TickAttribLast) { self.tick_attrib_last = v; }
    #[getter(specialConditions)]
    fn get_special_conditions(&self) -> String { self.special_conditions.clone() }
    #[setter(specialConditions)]
    fn set_special_conditions(&mut self, v: String) { self.special_conditions = v; }

    fn __repr__(&self) -> String {
        format!("HistoricalTickLast(time={}, price={}, size={}, exchange={})",
            self.time, self.price, self.size, self.exchange)
    }
}

/// One historical quote, both sides of it.
#[pyclass(from_py_object)]
#[derive(Clone, Default)]
pub struct HistoricalTickBidAsk {
    #[pyo3(get, set)]
    pub time: i64,
    #[pyo3(get, set)]
    pub tick_attrib_bid_ask: TickAttribBidAsk,
    #[pyo3(get, set)]
    pub price_bid: f64,
    #[pyo3(get, set)]
    pub price_ask: f64,
    #[pyo3(get, set)]
    pub size_bid: f64,
    #[pyo3(get, set)]
    pub size_ask: f64,
}

#[pymethods]
impl HistoricalTickBidAsk {
    #[new]
    #[pyo3(signature = (time=0, tick_attrib_bid_ask=TickAttribBidAsk::default(),
                        price_bid=0.0, price_ask=0.0, size_bid=0.0, size_ask=0.0))]
    fn new(
        time: i64, tick_attrib_bid_ask: TickAttribBidAsk,
        price_bid: f64, price_ask: f64, size_bid: f64, size_ask: f64,
    ) -> Self {
        Self { time, tick_attrib_bid_ask, price_bid, price_ask, size_bid, size_ask }
    }

    #[getter(tickAttribBidAsk)]
    fn get_tick_attrib_bid_ask(&self) -> TickAttribBidAsk { self.tick_attrib_bid_ask.clone() }
    #[setter(tickAttribBidAsk)]
    fn set_tick_attrib_bid_ask(&mut self, v: TickAttribBidAsk) { self.tick_attrib_bid_ask = v; }
    #[getter(priceBid)]
    fn get_price_bid(&self) -> f64 { self.price_bid }
    #[setter(priceBid)]
    fn set_price_bid(&mut self, v: f64) { self.price_bid = v; }
    #[getter(priceAsk)]
    fn get_price_ask(&self) -> f64 { self.price_ask }
    #[setter(priceAsk)]
    fn set_price_ask(&mut self, v: f64) { self.price_ask = v; }
    #[getter(sizeBid)]
    fn get_size_bid(&self) -> f64 { self.size_bid }
    #[setter(sizeBid)]
    fn set_size_bid(&mut self, v: f64) { self.size_bid = v; }
    #[getter(sizeAsk)]
    fn get_size_ask(&self) -> f64 { self.size_ask }
    #[setter(sizeAsk)]
    fn set_size_ask(&mut self, v: f64) { self.size_ask = v; }

    fn __repr__(&self) -> String {
        format!("HistoricalTickBidAsk(time={}, bid={}, ask={})",
            self.time, self.price_bid, self.price_ask)
    }
}

// The reference client's spelling for the three attribute objects. Its own
// sample reads `attrib.canAutoExecute`, `tickAttribLast.pastLimit` and
// `tickAttribBidAsk.bidPastLow`, and every other class here answers to both
// spellings. These answered to neither camel name, so the callback raised
// inside the dispatch loop, the loop logged it and carried on, and the tick
// never reached the program — no exception, no missing method, no tick.
camel_aliases_copy! {
    TickAttrib {
        get_can_auto_execute_alias set_can_auto_execute_alias
            canAutoExecute can_auto_execute bool;
        get_past_limit_alias set_past_limit_alias pastLimit past_limit bool;
        get_pre_open_alias set_pre_open_alias preOpen pre_open bool;
    }
}

camel_aliases_copy! {
    TickAttribLast {
        get_past_limit_alias set_past_limit_alias pastLimit past_limit bool;
    }
}

camel_aliases_copy! {
    TickAttribBidAsk {
        get_bid_past_low_alias set_bid_past_low_alias bidPastLow bid_past_low bool;
        get_ask_past_high_alias set_ask_past_high_alias askPastHigh ask_past_high bool;
    }
}
