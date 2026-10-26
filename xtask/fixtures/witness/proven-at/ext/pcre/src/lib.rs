//! A negative control (harness-design.md §4.1, §4.3; #3365, #3369): an
//! extension whose two witnesses a `trace:` row of the fixture's
//! `docs/divergence.toml` covers, the row naming `proven_at =
//! "optimizer-off"`. Each fence pins the linked engine's side and the
//! engine meets it. The first prints what the engine prints whatever the
//! oracle's optimizer does, so the oracle under the regime agrees and the
//! fence is covered; the second prints `oracle` under any php without
//! `zend_test`, which `php-linked`'s bundle carries and the oracle does
//! not load, so the oracle under the regime still differs and the fence
//! fails, naming its id.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// A native whose witness the proven row covers and the oracle agrees on.
///
/// ```php
/// echo 1;
/// ```
/// ```expect
/// 1
/// ```
pub fn preg_quote() {}

/// A native whose witness the proven row covers and the oracle differs on
/// under the regime too.
///
/// ```php
/// echo extension_loaded('zend_test') ? 'engine' : 'oracle';
/// ```
/// ```expect
/// engine
/// ```
pub fn preg_match() {}
