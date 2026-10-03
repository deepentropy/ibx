//! Golden codec tests (ibx#486): frames recorded between the reference
//! gateway and the servers, replayed through ibx, and compared with what the
//! reference did.
//!
//! - Decode: the recorded server frames go through the engine's links and
//!   the API client; ibx's callbacks are compared with the callbacks the
//!   reference's API client received (type, value, size, attributes, count,
//!   order).
//! - Encode: the recorded API requests are made again; the messages ibx
//!   writes are compared with the reference's, after the session fields are
//!   normalised (`test_support::normalise`).
//!
//! The fixtures are under tests/fixtures/gw1040/codec/ (`codec/1`, made by
//! scripts/codec_fixtures.py), read and replayed by
//! `test_support::scenario`.

mod account;
mod hmds;
mod l1;
mod orders;
mod replay;

use crate::test_support::scenario::{load_codec, Scenario};

/// A codec fixture by name.
fn load(name: &str) -> Scenario {
    load_codec(name)
}
