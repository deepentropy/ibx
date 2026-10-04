//! Scenario replay (ibx#486, ibx#487): a scenario recorded from the
//! reference gateway (the four legs: API requests, frames to the servers,
//! frames from the servers, API callbacks) replayed through ibx against a
//! scripted peer that plays the recorded servers.
//!
//! - [`record`]: the scenario files and the canonical form of a callback;
//! - [`session`]: the engine on in-memory links, the Rust API client on
//!   top, its callbacks as lines;
//! - [`request`]: a recorded API request made again on the Rust client;
//! - [`runner`]: the runner, its two checks (ibx's frames = the
//!   reference's frames, ibx's callbacks = the reference's callbacks) and
//!   the [`runner::Driver`] that lets another API client (the Python one)
//!   take the place of the Rust client.

pub mod record;
pub mod request;
pub mod runner;
pub mod session;

pub use record::{canonical, load_codec, load_path, load_scenario, Rec, Scenario};
pub use runner::{assert_same_callbacks, replay, run, Driver, Options, Outcome, RustDriver};
pub use session::{Links, Recorder, Session};
