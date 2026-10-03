//! Test helpers shared by the unit tests and the integration tests: a
//! scripted peer on the in-memory transport and the session-field
//! normaliser. Built only for the tests (`test-support` feature); never part
//! of a release build.

pub mod decoders;
pub mod normalise;
pub mod peer;

pub use normalise::{assert_same_fields, parse_fields, parse_pipe, to_pipe, Fields, Normaliser};
pub use peer::Peer;
