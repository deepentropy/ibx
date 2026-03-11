//! Shared Python-visible data types for the direct API.

use pyo3::prelude::*;

use crate::types::*;

pub const PRICE_SCALE_F: f64 = PRICE_SCALE as f64;

#[pyclass]
#[derive(Clone)]
pub struct PyQuote {
    #[pyo3(get)]
    pub bid: f64,
    #[pyo3(get)]
    pub ask: f64,
    #[pyo3(get)]
    pub last: f64,
    #[pyo3(get)]
    pub bid_size: f64,
    #[pyo3(get)]
    pub ask_size: f64,
    #[pyo3(get)]
    pub last_size: f64,
    #[pyo3(get)]
    pub volume: f64,
    #[pyo3(get)]
    pub open: f64,
    #[pyo3(get)]
    pub high: f64,
    #[pyo3(get)]
    pub low: f64,
    #[pyo3(get)]
    pub close: f64,
    #[pyo3(get)]
    pub timestamp_ns: u64,
}

#[pymethods]
impl PyQuote {
    fn __repr__(&self) -> String {
        format!("Quote(bid={}, ask={}, last={})", self.bid, self.ask, self.last)
    }
}

impl From<Quote> for PyQuote {
    fn from(q: Quote) -> Self {
        Self {
            bid: q.bid as f64 / PRICE_SCALE_F,
            ask: q.ask as f64 / PRICE_SCALE_F,
            last: q.last as f64 / PRICE_SCALE_F,
            bid_size: q.bid_size as f64 / QTY_SCALE as f64,
            ask_size: q.ask_size as f64 / QTY_SCALE as f64,
            last_size: q.last_size as f64 / QTY_SCALE as f64,
            volume: q.volume as f64 / QTY_SCALE as f64,
            open: q.open as f64 / PRICE_SCALE_F,
            high: q.high as f64 / PRICE_SCALE_F,
            low: q.low as f64 / PRICE_SCALE_F,
            close: q.close as f64 / PRICE_SCALE_F,
            timestamp_ns: q.timestamp_ns,
        }
    }
}

#[pyclass]
#[derive(Clone)]
pub struct PyFill {
    #[pyo3(get)]
    pub instrument: u32,
    #[pyo3(get)]
    pub order_id: u64,
    #[pyo3(get)]
    pub side: String,
    #[pyo3(get)]
    pub price: f64,
    #[pyo3(get)]
    pub qty: i64,
    #[pyo3(get)]
    pub remaining: i64,
    #[pyo3(get)]
    pub commission: f64,
    #[pyo3(get)]
    pub timestamp_ns: u64,
}

#[pymethods]
impl PyFill {
    fn __repr__(&self) -> String {
        format!("Fill(order_id={}, side={}, price={}, qty={})", self.order_id, self.side, self.price, self.qty)
    }
}

impl From<Fill> for PyFill {
    fn from(f: Fill) -> Self {
        Self {
            instrument: f.instrument,
            order_id: f.order_id,
            side: match f.side { Side::Buy => "BUY".into(), Side::Sell => "SELL".into(), Side::ShortSell => "SSHORT".into() },
            price: f.price as f64 / PRICE_SCALE_F,
            qty: f.qty,
            remaining: f.remaining,
            commission: f.commission as f64 / PRICE_SCALE_F,
            timestamp_ns: f.timestamp_ns,
        }
    }
}

#[pyclass]
#[derive(Clone)]
pub struct PyOrderUpdate {
    #[pyo3(get)]
    pub order_id: u64,
    #[pyo3(get)]
    pub instrument: u32,
    #[pyo3(get)]
    pub status: String,
    #[pyo3(get)]
    pub filled_qty: i64,
    #[pyo3(get)]
    pub remaining_qty: i64,
    #[pyo3(get)]
    pub timestamp_ns: u64,
}

#[pymethods]
impl PyOrderUpdate {
    fn __repr__(&self) -> String {
        format!("OrderUpdate(order_id={}, status={})", self.order_id, self.status)
    }
}

impl From<OrderUpdate> for PyOrderUpdate {
    fn from(u: OrderUpdate) -> Self {
        Self {
            order_id: u.order_id,
            instrument: u.instrument,
            status: format!("{:?}", u.status),
            filled_qty: u.filled_qty,
            remaining_qty: u.remaining_qty,
            timestamp_ns: u.timestamp_ns,
        }
    }
}

#[pyclass]
#[derive(Clone)]
pub struct PyAccountState {
    #[pyo3(get)]
    pub net_liquidation: f64,
    #[pyo3(get)]
    pub buying_power: f64,
    #[pyo3(get)]
    pub margin_used: f64,
    #[pyo3(get)]
    pub unrealized_pnl: f64,
    #[pyo3(get)]
    pub realized_pnl: f64,
}

#[pymethods]
impl PyAccountState {
    fn __repr__(&self) -> String {
        format!("AccountState(net_liq={}, buying_power={})", self.net_liquidation, self.buying_power)
    }
}

impl From<AccountState> for PyAccountState {
    fn from(a: AccountState) -> Self {
        Self {
            net_liquidation: a.net_liquidation as f64 / PRICE_SCALE_F,
            buying_power: a.buying_power as f64 / PRICE_SCALE_F,
            margin_used: a.margin_used as f64 / PRICE_SCALE_F,
            unrealized_pnl: a.unrealized_pnl as f64 / PRICE_SCALE_F,
            realized_pnl: a.realized_pnl as f64 / PRICE_SCALE_F,
        }
    }
}

/// Parse side string into Side enum.
pub fn parse_side(s: &str) -> PyResult<Side> {
    match s.to_uppercase().as_str() {
        "BUY" | "B" => Ok(Side::Buy),
        "SELL" | "S" => Ok(Side::Sell),
        "SSHORT" | "SS" => Ok(Side::ShortSell),
        _ => Err(pyo3::exceptions::PyRuntimeError::new_err(format!("Invalid side '{}': use 'BUY' or 'SELL'", s))),
    }
}
