//! Small values of callbacks that the official client library gives as
//! objects with named attributes: a family code, a price increment of a
//! market rule, a session of a historical schedule.
//!
//! Each is a tuple with names: `code.accountID` as with the official
//! library, and `account, code = entry` as the client gave them before
//! (plain tuples).

use pyo3::ffi::c_str;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;

const SOURCE: &std::ffi::CStr = c_str!(r#"
import collections
import sys


def _number(v):
    return "" if v == sys.float_info.max else f"{v:.8f}".rstrip("0").rstrip(".")


class FamilyCode(collections.namedtuple("FamilyCode", "accountID familyCodeStr", defaults=("", ""))):
    """An account and its family code, as the official `FamilyCode`."""
    __slots__ = ()

    def __str__(self):
        return f"AccountId: {self.accountID}, FamilyCodeStr: {self.familyCodeStr}"


class PriceIncrement(collections.namedtuple("PriceIncrement", "lowEdge increment", defaults=(0.0, 0.0))):
    """A price increment of a market rule, from the price `lowEdge` up, as the official `PriceIncrement`."""
    __slots__ = ()

    def __str__(self):
        return f"LowEdge: {_number(self.lowEdge)}, Increment: {_number(self.increment)}"


class HistoricalSession(collections.namedtuple("HistoricalSession", "refDate startDateTime endDateTime", defaults=("", "", ""))):
    """A session of a historical schedule, as the official `HistoricalSession`."""
    __slots__ = ()

    def __str__(self):
        return f"Start: {self.startDateTime}, End: {self.endDateTime}, Ref Date: {self.refDate}"
"#);

const NAMES: [&str; 3] = ["FamilyCode", "PriceIncrement", "HistoricalSession"];

static CLASSES: PyOnceLock<[Py<PyAny>; 3]> = PyOnceLock::new();

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    let module = PyModule::from_code(py, SOURCE, c"named.py", c"ibx.named")?;
    let mut classes = Vec::with_capacity(NAMES.len());
    for name in NAMES {
        let class = module.getattr(name)?;
        class.setattr("__module__", "ibx")?;
        m.add(name, &class)?;
        classes.push(class.unbind());
    }
    let classes: [Py<PyAny>; 3] = classes.try_into().map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("named values"))?;
    let _ = CLASSES.set(py, classes);
    Ok(())
}

fn class<'py>(py: Python<'py>, index: usize) -> PyResult<&'py Bound<'py, PyAny>> {
    CLASSES.get(py).map(|c| c[index].bind(py))
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("the named values are not registered"))
}

pub(crate) fn family_code<'py>(py: Python<'py>, account_id: &str, family_code: &str) -> PyResult<Bound<'py, PyAny>> {
    class(py, 0)?.call1((account_id, family_code))
}

pub(crate) fn price_increment<'py>(py: Python<'py>, low_edge: f64, increment: f64) -> PyResult<Bound<'py, PyAny>> {
    class(py, 1)?.call1((low_edge, increment))
}

pub(crate) fn historical_session<'py>(py: Python<'py>, ref_date: &str, start: &str, end: &str) -> PyResult<Bound<'py, PyAny>> {
    class(py, 2)?.call1((ref_date, start, end))
}
