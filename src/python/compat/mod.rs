//! ibapi-compatible layer: EWrapper, EClient, Contract, Order, tick types.

pub mod client;
pub mod contract;
pub mod tick_types;
pub mod wrapper;

use pyo3::prelude::*;

/// Register all compat classes on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    contract::register(m)?;
    tick_types::register(m)?;
    wrapper::register(m)?;
    client::register(m)?;
    Ok(())
}
