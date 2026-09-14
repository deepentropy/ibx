//! What the venue reports back: fills, their cost, bars, and news.

// The other families, and the two helpers every class here uses.
use super::contract::{by_reference_name, enum_code, enum_member, set_by_reference_name};
use pyo3::prelude::*;

use super::camel_aliases_copy;

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
    #[pyo3(get, set)]
    pub volume: i64,
    #[pyo3(get, set)]
    pub wap: f64,
    #[pyo3(get, set)]
    pub bar_count: i32,
    /// Which timezone `date` is stated in, as the reply states it. Without
    /// it the timestamp says nothing about what the bar times mean. Empty on
    /// streaming updates, which carry no timezone of their own.
    #[pyo3(get, set)]
    pub timezone: String,
    /// When the bar closed, as the venue states it.
    ///
    /// A bar the venue aggregated states its own bounds — a week runs Monday to
    /// Friday, a month the first to the last — and those are not derivable from
    /// the start. The last bar of any series is normally partial, so this is
    /// what tells a finished week from a running one. Empty where the venue
    /// stated none, which is every size shorter than a week.
    #[pyo3(get, set)]
    pub end: String,
}

#[pymethods]
impl BarData {
    /// Answer to the name the reference client gives a field as well as the
    /// name this one gives it.
    ///
    /// This object is handed to a caller by a callback and only ever read. Code
    /// written for the reference client reads the run-together names, and under
    /// this class they were absent — so the object arrived carrying everything
    /// and answered nothing.
    ///
    /// Only reached when the attribute was not found, so it costs nothing on
    /// the names this class defines.
    fn __getattr__(slf: Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        by_reference_name(slf.as_any(), name, &[])
    }

    /// The same names, written to: a field set under the reference client's
    /// spelling lands on this client's field.
    fn __setattr__(slf: Bound<'_, Self>, name: &str, value: Bound<'_, PyAny>) -> PyResult<()> {
        set_by_reference_name(slf.as_any(), name, &value, &[])
    }

    #[new]
    #[pyo3(signature = (date="".to_string(), open=0.0, high=0.0, low=0.0, close=0.0, volume=0, wap=0.0, bar_count=0, timezone="".to_string(), end="".to_string()))]
    pub fn new(date: String, open: f64, high: f64, low: f64, close: f64, volume: i64, wap: f64, bar_count: i32, timezone: String, end: String) -> Self {
        Self { date, open, high, low, close, volume, wap, bar_count, timezone, end }
    }

    fn __repr__(&self) -> String {
        format!("BarData(date='{}', O={}, H={}, L={}, C={}, V={})",
            self.date, self.open, self.high, self.low, self.close, self.volume)
    }
}

/// ibapi-compatible Execution class (used in exec_details callback).
#[pyclass(from_py_object)]
#[derive(Clone, Debug, Default)]
pub struct Execution {
    /// Every field the report stated that this client does not name, as
    /// (tag, value). Kept rather than dropped.
    #[pyo3(get, set)]
    pub unnamed_fields: Vec<(u32, String)>,
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
    #[pyo3(get, set)]
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
    #[pyo3(get, set)]
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
    /// Who entered the order this fill belongs to, as the report names them.
    #[pyo3(get, set)]
    pub submitter: String,
    /// How an option's fill came about, as the reference client codes it;
    /// -1 where none is stated, and this client reads none off a report. Read
    /// and written as that client's `OptionExerciseType` member.
    pub opt_exercise_or_lapse_type: i32,
}

impl Execution {
    /// The same execution in the shape a caller reads.
    ///
    /// Every field, so the record stored for a replay and the object announced
    /// on the callback cannot differ: built separately, one carried the
    /// caller's own label for the order and the other did not, and a replay
    /// answered with a blank where the live callback had stated it.
    pub(crate) fn from_api(e: &crate::types::model::Execution) -> Self {
        Self {
            unnamed_fields: e.unnamed_fields.clone(),
            exec_id: e.exec_id.clone(),
            time: e.time.clone(),
            acct_number: e.acct_number.clone(),
            exchange: e.exchange.clone(),
            side: e.side.clone(),
            shares: e.shares,
            price: e.price,
            perm_id: e.perm_id,
            client_id: e.client_id,
            order_id: e.order_id,
            liquidation: e.liquidation,
            cum_qty: e.cum_qty,
            avg_price: e.avg_price,
            order_ref: e.order_ref.clone(),
            ev_rule: e.ev_rule.clone(),
            ev_multiplier: e.ev_multiplier,
            model_code: e.model_code.clone(),
            last_liquidity: e.last_liquidity,
            pending_price_revision: e.pending_price_revision,
            submitter: e.submitter.clone(),
            opt_exercise_or_lapse_type: -1,
        }
    }
}

#[pymethods]
impl Execution {
    /// Answer to the name the reference client gives a field as well as the
    /// name this one gives it.
    ///
    /// This object is handed to a caller by a callback and only ever read. Code
    /// written for the reference client reads the run-together names, and under
    /// this class they were absent — so the object arrived carrying everything
    /// and answered nothing.
    ///
    /// Only reached when the attribute was not found, so it costs nothing on
    /// the names this class defines.
    fn __getattr__(slf: Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        by_reference_name(slf.as_any(), name, &[])
    }

    /// The same names, written to: a field set under the reference client's
    /// spelling lands on this client's field.
    fn __setattr__(slf: Bound<'_, Self>, name: &str, value: Bound<'_, PyAny>) -> PyResult<()> {
        set_by_reference_name(slf.as_any(), name, &value, &[])
    }

    #[getter]
    fn get_opt_exercise_or_lapse_type(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        enum_member(py, "OptionExerciseType", self.opt_exercise_or_lapse_type)
    }
    #[setter]
    fn set_opt_exercise_or_lapse_type(&mut self, value: &Bound<'_, PyAny>) -> PyResult<()> {
        self.opt_exercise_or_lapse_type = enum_code(value)?.extract()?;
        Ok(())
    }

    #[new]
    #[pyo3(signature = ())]
    fn new() -> Self { Self { opt_exercise_or_lapse_type: -1, ..Self::default() } }
}

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

/// ibapi-compatible HistogramData class.
///
/// The reference client hands a histogram over as objects carrying `price` and
/// `size` rather than as pairs. Code written against it reads `item.price`, and
/// a pair answers nothing — the attribute error is caught by the callback
/// dispatcher, so the caller was handed a histogram it could not read and heard
/// no complaint about it either.
#[pyclass(from_py_object, name = "HistogramData")]
#[derive(Clone, Debug, Default)]
pub struct HistogramDataPy {
    #[pyo3(get, set)]
    pub price: f64,
    #[pyo3(get, set)]
    pub size: f64,
}

#[pymethods]
impl HistogramDataPy {
    #[new]
    #[pyo3(signature = ())]
    fn new() -> Self { Self::default() }
}

/// ibapi-compatible FamilyCode class.
///
/// Stated as the reference client states one, for the reason `HistogramData` is:
/// a program reads `code.accountID`, which a pair does not carry.
#[pyclass(from_py_object, name = "FamilyCode")]
#[derive(Clone, Debug, Default)]
pub struct FamilyCodePy {
    #[pyo3(get, set)]
    pub account_id: String,
    #[pyo3(get, set)]
    pub family_code_str: String,
}

#[pymethods]
impl FamilyCodePy {
    /// Answer to the name the reference client gives a field as well as the
    /// name this one gives it.
    fn __getattr__(slf: Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        by_reference_name(slf.as_any(), name, &[])
    }

    /// The same names, written to.
    fn __setattr__(slf: Bound<'_, Self>, name: &str, value: Bound<'_, PyAny>) -> PyResult<()> {
        set_by_reference_name(slf.as_any(), name, &value, &[])
    }

    #[new]
    #[pyo3(signature = ())]
    fn new() -> Self { Self::default() }
}

/// ibapi-compatible HistoricalSession class.
///
/// The reference client states a session as `startDateTime`, `endDateTime` and
/// `refDate`. Handed over as a triple this client ordered its own way, a program
/// written against that client did not merely fail to read it — it read the
/// reference date as the opening time.
#[pyclass(from_py_object, name = "HistoricalSession")]
#[derive(Clone, Debug, Default)]
pub struct HistoricalSessionPy {
    #[pyo3(get, set)]
    pub start_date_time: String,
    #[pyo3(get, set)]
    pub end_date_time: String,
    #[pyo3(get, set)]
    pub ref_date: String,
}

#[pymethods]
impl HistoricalSessionPy {
    /// Answer to the name the reference client gives a field as well as the
    /// name this one gives it.
    fn __getattr__(slf: Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        by_reference_name(slf.as_any(), name, &[])
    }

    /// The same names, written to.
    fn __setattr__(slf: Bound<'_, Self>, name: &str, value: Bound<'_, PyAny>) -> PyResult<()> {
        set_by_reference_name(slf.as_any(), name, &value, &[])
    }

    #[new]
    #[pyo3(signature = ())]
    fn new() -> Self { Self::default() }
}

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
    #[pyo3(get, set)]
    pub yield_redemption_date: i64,
}

#[pymethods]
impl CommissionAndFeesReport {
    #[new]
    #[pyo3(signature = ())]
    fn new() -> Self { Self::default() }

    // Under the reference client's own names. A program reads a commission
    // report by the names its library declares, and this one is handed
    // straight to that library's callback: a name it does not answer to is an
    // exception on every fill, which is every time money moves.
    #[getter(execId)]
    fn get_exec_id_alias(&self) -> String { self.exec_id.clone() }
    #[setter(execId)]
    fn set_exec_id_alias(&mut self, v: String) { self.exec_id = v; }

    /// What the venue charged. The reference client calls the whole of it the
    /// commission; this client names it for what it now includes.
    #[getter(commission)]
    fn get_commission_alias(&self) -> f64 { self.commission_and_fees }
    #[setter(commission)]
    fn set_commission_alias(&mut self, v: f64) { self.commission_and_fees = v; }



    /// Their spelling keeps the underscore: `yield` is a keyword in Python.
    #[getter(yield_)]
    fn get_yield_alias(&self) -> f64 { self.yield_amount }
    #[setter(yield_)]
    fn set_yield_alias(&mut self, v: f64) { self.yield_amount = v; }

}

camel_aliases_copy! {
    CommissionAndFeesReport {
        get_commission_and_fees_alias set_commission_and_fees_alias commissionAndFees commission_and_fees f64;
        get_realized_pnl_alias set_realized_pnl_alias realizedPNL realized_pnl f64;
        get_yield_redemption_date_alias set_yield_redemption_date_alias yieldRedemptionDate yield_redemption_date i64;
    }
}
