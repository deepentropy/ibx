//! The protobuf form of the requests and callbacks of the Python client
//! (ibx#725).
//!
//! The form is written in Python (`proto_forms.py`, with the tables of
//! `proto_table.py` that `scripts/proto_forms_table.py` writes): it reads
//! and makes objects of the official client library when that library is
//! installed, and nothing of it is on the path of a wrapper that defines no
//! protobuf callback.

use pyo3::ffi::c_str;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;

/// The Python module of the form, made once at the registration of `ibx`.
static FORMS: PyOnceLock<Py<PyModule>> = PyOnceLock::new();

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    let table = PyModule::from_code(py, c_str!(include_str!("proto_table.py")), c"proto_table.py", c"ibx.proto_table")?;
    let forms = PyModule::from_code(py, c_str!(include_str!("proto_forms.py")), c"proto_forms.py", c"ibx.proto_forms")?;
    forms.getattr("install")?.call1((m, &table))?;
    // Public: `ibx.proto_forms` (its `make`, `ProtoForm`, `official`, `OFFICIAL_NAMES`).
    m.add("proto_forms", &forms)?;
    let _ = FORMS.set(py, forms.unbind());
    Ok(())
}

/// What the client calls for `wrapper`: the wrapper itself, or the layer
/// that gives it its protobuf callbacks when it defines any.
pub(crate) fn callbacks(py: Python<'_>, wrapper: &Py<PyAny>) -> Py<PyAny> {
    let made = FORMS.get(py)
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("the protobuf form is not registered"))
        .and_then(|forms| forms.bind(py).getattr("callbacks")?.call1((wrapper,)));
    match made {
        Ok(callbacks) => callbacks.unbind(),
        Err(e) => {
            log::error!("the protobuf callbacks of the wrapper are left out: {}", e);
            wrapper.clone_ref(py)
        }
    }
}
