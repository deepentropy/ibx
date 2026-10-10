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
    // Forward Rust `log::*` macros to stderr when RUST_LOG is set.
    // `try_init` is no-op if a logger is already installed (e.g. by tests).
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp_millis()
        .try_init();
    compat::register(m)?;
    Ok(())
}
