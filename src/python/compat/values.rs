//! The official API's value conventions in Python: its unset Decimal, its
//! list attributes, its camelCase attribute names.

use pyo3::prelude::*;
use pyo3::types::{PyList, PyString};

/// The official API's unset Decimal (`ibapi.const.UNSET_DECIMAL`, 2**127 - 1).
pub const UNSET_DECIMAL_TEXT: &str = "170141183460469231731687303715884105727";

/// The unset Decimal as a double (2**127), what `float(UNSET_DECIMAL)` gives.
const UNSET_DECIMAL_F64: f64 = 170141183460469231731687303715884105727.0;

fn decimal_class(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    py.import("decimal")?.getattr("Decimal")
}

/// A quantity (the official API's Decimal) as a `decimal.Decimal`: ibx's
/// unset quantity (`f64::MAX`) is the API's unset Decimal.
pub fn decimal_to_py(py: Python<'_>, v: f64) -> PyResult<Py<PyAny>> {
    let text = if v == f64::MAX { UNSET_DECIMAL_TEXT.to_string() } else { format!("{}", v) };
    Ok(decimal_class(py)?.call1((text,))?.unbind())
}

/// A quantity from Python: a Decimal, an int, a float or a text. The API's
/// unset Decimal and the maximum double are unset (`f64::MAX`).
pub fn decimal_from_py(v: &Bound<'_, PyAny>) -> PyResult<f64> {
    let f: f64 = match v.cast::<PyString>() {
        Ok(s) => s.to_str()?.trim().parse()
            .map_err(|_| pyo3::exceptions::PyValueError::new_err(format!("not a quantity: '{}'", s)))?,
        Err(_) => v.extract()?,
    };
    Ok(if f == UNSET_DECIMAL_F64 { f64::MAX } else { f })
}

/// A quantity carried as text as a `decimal.Decimal`: empty is the API's
/// unset Decimal.
pub fn decimal_text_to_py(py: Python<'_>, v: &str) -> PyResult<Py<PyAny>> {
    let text = if v.is_empty() { UNSET_DECIMAL_TEXT } else { v };
    Ok(decimal_class(py)?.call1((text,))?.unbind())
}

/// A quantity carried as text from Python: the API's unset Decimal is
/// empty.
pub fn decimal_text_from_py(v: &Bound<'_, PyAny>) -> PyResult<String> {
    let text = v.str()?.to_string();
    Ok(if text == UNSET_DECIMAL_TEXT { String::new() } else { text })
}

/// A new empty list, the official API's `[]` default.
pub fn empty_list() -> Py<PyAny> {
    Python::attach(|py| PyList::empty(py).into_any().unbind())
}

/// A list of the given objects, or None when there is none (the official
/// API's default of its optional lists).
pub fn list_or_none<T: for<'py> IntoPyObject<'py>>(py: Python<'_>, items: Vec<T>) -> PyResult<Py<PyAny>> {
    if items.is_empty() {
        return Ok(py.None());
    }
    Ok(PyList::new(py, items)?.into_any().unbind())
}

/// The items of a list attribute (any iterable); None has no item.
pub fn items<'py>(py: Python<'py>, obj: &Py<PyAny>) -> Vec<Bound<'py, PyAny>> {
    if obj.is_none(py) {
        return Vec::new();
    }
    obj.bind(py).try_iter()
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// Read an attribute of an object of the official API's or of this module's
/// shape: the camelCase name, else the snake_case one.
pub fn attr<'py, T: for<'a> pyo3::FromPyObject<'a, 'py>>(obj: &Bound<'py, PyAny>, camel: &str, snake: &str) -> Option<T> {
    obj.getattr(camel).or_else(|_| obj.getattr(snake)).ok().and_then(|v| v.extract::<T>().ok())
}

/// The official API's camelCase attribute names of a class whose fields
/// have ibx's snake_case names: each one reads and writes its snake_case
/// attribute (`__getattr__` is only asked when the normal lookup fails).
macro_rules! official_names {
    ($ty:ty, [$(($camel:literal, $snake:literal)),* $(,)?]) => {
        #[pymethods]
        impl $ty {
            fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
                const NAMES: &[(&str, &str)] = &[$(($camel, $snake)),*];
                match NAMES.iter().find(|(camel, _)| *camel == name) {
                    Some((_, snake)) => Ok(slf.getattr(*snake)?.unbind()),
                    None => Err(pyo3::exceptions::PyAttributeError::new_err(format!(
                        "'{}' object has no attribute '{}'", slf.get_type().name()?, name))),
                }
            }

            fn __setattr__(slf: &Bound<'_, Self>, name: &str, value: Bound<'_, PyAny>) -> PyResult<()> {
                const NAMES: &[(&str, &str)] = &[$(($camel, $snake)),*];
                let name = NAMES.iter().find(|(camel, _)| *camel == name).map_or(name, |(_, snake)| *snake);
                let name = pyo3::types::PyString::new(slf.py(), name);
                // The generic attribute set: the field's own setter.
                let rc = unsafe { pyo3::ffi::PyObject_GenericSetAttr(slf.as_ptr(), name.as_ptr(), value.as_ptr()) };
                if rc == 0 { Ok(()) } else { Err(PyErr::fetch(slf.py())) }
            }
        }
    };
}
pub(crate) use official_names;
