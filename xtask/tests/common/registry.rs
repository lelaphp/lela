//! The divergence registry a test reads, parsed from the text it states.
//! Each file of `tests/common` is one topic: an integration test includes
//! the topics it calls by name, as `mod common { pub mod registry; }`, and
//! the unit tests under `src` include theirs once, in lib.rs, by `#[path]`.
//! A binary takes a topic whole and may call part of it.
#![allow(
    dead_code,
    reason = "a binary includes the topic whole and calls part of it"
)]

use xtask::divergence::Registry;

/// The registry `text` states; a fixture that does not parse fails the
/// test that wrote it.
pub fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}
