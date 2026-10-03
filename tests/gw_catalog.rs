//! Layer A of the proof that ibx behaves like the gateway 1040 (ibx#485):
//! table tests whose expected values all come from the catalogs extracted
//! from the gateway 1040 JAR (`tests/fixtures/gw1040/catalog/`). Offline,
//! through the real ibx code paths, on the in-memory transport.

#[path = "gw_catalog/catalog.rs"]
mod catalog;
#[path = "gw_catalog/harness.rs"]
mod harness;
#[path = "gw_catalog/writers.rs"]
mod writers;
#[path = "gw_catalog/errors.rs"]
mod errors;
