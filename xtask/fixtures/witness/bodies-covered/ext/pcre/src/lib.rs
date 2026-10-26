//! A negative control (harness-design.md §4.1, §4.3, §8; #3455): an
//! extension whose one witness pins the engine's side of the fixture's
//! `trace:` row in `docs/divergence.toml`, and which the oracle does not
//! meet: the snippet prints `engine` where `zend_test` is loaded, which is
//! `php-linked`, and `oracle` where it is not. `xtask witness --accept
//! --bodies` must refuse to write the oracle's streams into it, naming
//! the row, since its body is the engine's side and not the oracle's; the
//! fence stays covered, and this file stands as it is.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// A native whose witness pins the engine's side of the fixture's row.
///
/// ```php
/// echo extension_loaded('zend_test') ? 'engine' : 'oracle';
/// ```
/// ```expect
/// engine
/// ```
pub fn preg_quote() {}
