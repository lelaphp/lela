//! A negative control (#1493): an extension whose one native carries a
//! `request fpm` witness, run where the oracle's php-fpm reads the whole
//! request and closes the connection without a byte of response. The run
//! must fail as that witness's bork, in the transport's words, and must
//! not read the native as one without a witness.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::attr::php_function;

/// A native served by php-fpm, whose fence the server answers with nothing.
///
/// ```php
/// var_dump(fixture_finish());
/// ```
/// ```request fpm
/// --GET--
/// q=1
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
pub fn fixture_finish() -> bool {
    true
}
