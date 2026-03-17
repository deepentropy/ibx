//! PyO3 bindings for ibx. Feature-gated behind `python`.
//!
//! Provides an ibapi-compatible API (callback-based):
//! ```python
//! from ibx import EClient, EWrapper, Contract, Order
//! class App(EWrapper):
//!     def next_valid_id(self, order_id):
//!         ...
//! app = App()
//! client = EClient(app)
//! client.connect(username="user", password="pass", paper=True)
//! client.run()
//! ```

mod types;
pub mod compat;

use pyo3::prelude::*;

/// Python module definition.
#[pymodule]
fn ibx(m: &Bound<'_, PyModule>) -> PyResult<()> {
    compat::register(m)?;
    Ok(())
}
