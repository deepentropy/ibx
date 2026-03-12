//! ibapi-compatible Rust API.
//!
//! Provides `Contract`, `Order`, `Wrapper` trait, and `EClient` — matching the
//! C++ TWS API (EClientSocket / EWrapper) pattern.

pub mod types;

pub use types::*;
