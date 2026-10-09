//! A negative control (harness-design.md §4.1, §4.3, §8; #1650, #1728):
//! an extension whose two witnesses end the oracle on `SIGKILL` and print
//! nothing under the linked engine. The snippet sends the signal only where
//! `zend_test` is not loaded, which is the oracle: `php-linked`'s bundle
//! carries it. The first fence is covered by the fixture's `trace:` row in
//! `docs/divergence.toml`, so it is held to its pinned `expect` against the
//! engine alone and the oracle's signal is recorded, not judged; the second
//! is covered by nothing, so it is a bork, and `xtask witness` fails the
//! crate on it.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// A native whose witness pins the engine's side of the fixture's row.
///
/// ```php
/// if (!extension_loaded('zend_test')) { exec('kill -KILL ' . getmypid()); sleep(60); }
/// ```
/// ```expect
/// ```
pub fn preg_quote() {}

/// A native whose witness ends the oracle on a signal with no row behind
/// it.
///
/// ```php
/// // No row covers this fence.
/// if (!extension_loaded('zend_test')) { exec('kill -KILL ' . getmypid()); sleep(60); }
/// ```
/// ```expect
/// ```
pub fn preg_match() {}
