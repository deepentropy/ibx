//! Test helpers shared by the unit tests and the integration tests: a
//! scripted peer on the in-memory transport, the session-field normaliser
//! and the scenario replay. Built only for the tests (`test-support`
//! feature); never part of a release build.

pub mod decoders;
pub mod normalise;
pub mod peer;
pub mod scenario;

pub use normalise::{assert_same_fields, parse_fields, parse_pipe, to_pipe, Fields, Normaliser};
pub use peer::Peer;
