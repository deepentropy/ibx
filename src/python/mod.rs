//! PyO3 bindings for ibx. Feature-gated behind `python`.
//!
//! Provides two APIs:
//!
//! **Direct API** (low-level, polling):
//! ```python
//! import ibx
//! engine = ibx.connect(username="user", password="pass", paper=True)
//! spy = engine.subscribe(conid=756733, symbol="SPY")
//! quote = engine.quote(spy)
//! order_id = engine.submit_limit(spy, "BUY", qty=1, price=680.50)
//! fills = engine.fills()
//! engine.shutdown()
//! ```
//!
//! **ibapi-compatible API** (callback-based):
//! ```python
//! from ibx import EClient, EWrapper, Contract, Order
//! class App(EWrapper, EClient):
//!     def __init__(self):
//!         EWrapper.__init__(self)
//!         EClient.__init__(self, wrapper=self)
//!     def nextValidId(self, orderId):
//!         ...
//! app = App()
//! app.connect(username="user", password="pass")
//! app.run()
//! ```

mod engine;
mod types;
pub mod compat;

use pyo3::prelude::*;
use crate::types::{PRICE_SCALE, QTY_SCALE};

/// Python module definition.
#[pymodule]
fn ibx(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Direct API
    m.add_function(pyo3::wrap_pyfunction!(engine::connect, m)?)?;
    m.add_class::<engine::IbEngine>()?;
    m.add_class::<types::PyQuote>()?;
    m.add_class::<types::PyFill>()?;
    m.add_class::<types::PyOrderUpdate>()?;
    m.add_class::<types::PyAccountState>()?;
    m.add("PRICE_SCALE", PRICE_SCALE)?;
    m.add("QTY_SCALE", QTY_SCALE)?;

    // ibapi-compatible API
    compat::register(m)?;

    Ok(())
}
