pub mod api;
pub mod auth;
pub mod bridge;
pub mod client_core;
pub mod config;
pub mod control;
pub mod gateway;
pub mod protocol;
pub mod types;

/// Internal engine module. Use [`api::EClient`] for the public API.
#[doc(hidden)]
pub mod engine;

#[cfg(feature = "python")]
mod python;

// Re-exports for convenience.
pub use api::{EClient, EClientConfig, Wrapper};
