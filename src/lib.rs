pub mod api;
pub mod auth;
pub mod bridge;
pub mod client_core;
pub mod config;
pub mod control;
pub mod gateway;
pub mod logging;
pub mod protocol;
pub mod types;

/// Internal engine module. Use [`api::EClient`] for the public API.
#[doc(hidden)]
pub mod engine;

#[cfg(feature = "python")]
mod python;

/// Test helpers (in-memory peer, normaliser); built only for the tests.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support;

// Re-exports for convenience.
pub use api::{EClient, EClientConfig, Wrapper};
