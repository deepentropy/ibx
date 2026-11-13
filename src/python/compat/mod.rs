//! ibapi-compatible layer: EWrapper, EClient, Contract, Order, tick types.

pub mod client;
pub mod config;
pub mod contract;
pub(crate) mod named;
pub(crate) mod proto_forms;
pub mod tick_types;
pub mod values;
pub mod wrapper;

use pyo3::prelude::*;

/// The attribute list of an object as the official one gives it: the second
/// spelling of a name (`con_id` beside `conId`) stays usable but is not
/// listed (ibx#697, decision of 15/10/2026). `hidden`: the class's second
/// spellings that are not the official name in snake case.
pub(crate) fn official_dir(obj: &Bound<'_, PyAny>, hidden: &[&str]) -> PyResult<Vec<String>> {
    let names: Vec<String> = obj.py().import("builtins")?.getattr("object")?.getattr("__dir__")?.call1((obj,))?.extract()?;
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for n in &names {
        if hidden.contains(&n.as_str()) {
            continue;
        }
        if n.starts_with('_') || !n.contains('_') {
            out.push(n.clone());
            continue;
        }
        // The official spelling the object answers to, in place of this one:
        // in camel case, or with its last word in capitals
        // (`init_margin_after_outside_rth`, `initMarginAfterOutsideRTH`).
        let official = [camel(n), acronym_last(n)].into_iter()
            .find(|c| names.contains(c) || obj.hasattr(c.as_str()).unwrap_or(false));
        out.push(official.unwrap_or_else(|| n.clone()));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// `last_trade_date` as `lastTradeDate`.
fn camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = false;
    for c in name.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// `realized_pnl` as `realizedPNL`.
fn acronym_last(name: &str) -> String {
    match name.rsplit_once('_') {
        Some((head, last)) => camel(head) + &last.to_uppercase(),
        None => name.to_string(),
    }
}

/// `__dir__` of the classes that mirror the official objects (ibx#697),
/// each with its own second spellings in brackets.
macro_rules! hide_second_spelling {
    ($($t:ty $([$($h:literal),* $(,)?])?),* $(,)?) => {
        $(
            #[pyo3::pymethods]
            impl $t {
                fn __dir__(slf: &pyo3::Bound<'_, Self>) -> pyo3::PyResult<Vec<String>> {
                    crate::python::compat::official_dir(slf.as_any(), &[$($($h),*)?])
                }
            }
        )*
    };
}
pub(crate) use hide_second_spelling;

/// Register all compat classes on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    contract::register(m)?;
    tick_types::register(m)?;
    wrapper::register(m)?;
    client::register(m)?;
    named::register(m)?;
    // After the classes: it adds the protobuf requests and callbacks to them.
    proto_forms::register(m)?;
    Ok(())
}
