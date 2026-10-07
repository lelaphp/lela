//! A negative control (harness-design.md §4.1; #1037, #1043, #2166): an
//! extension whose first witness ends the process serving it on
//! `SIGABRT`. `xtask witness` fails the crate with a reading that names the
//! signal and the witness, and serves and judges the witness after it with
//! a binary started again. The snippet sends the signal only under
//! `php-linked`, whose bundle carries `zend_test`; the oracle does not, and
//! prints nothing, so the oracle's run is whole and `php-linked` is served.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// A native whose witness kills the process that serves it. The `sleep`
/// keeps the snippet in flight until the signal lands, which a signal sent
/// to the process does on whichever thread takes it, not always before
/// `exec` returns.
///
/// ```php
/// if (extension_loaded('zend_test')) { exec('kill -ABRT ' . getmypid()); sleep(60); }
/// ```
/// ```expect
/// ```
pub fn preg_quote() {}

/// A native whose witness is served after the one above killed its
/// binary, by one started again.
///
/// ```php
/// echo 2;
/// ```
/// ```expect
/// 2
/// ```
pub fn preg_match() {}
