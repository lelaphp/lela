//! A negative control (harness-design.md §4.1, §8; #1890): an extension read
//! under the oracle variant `drift`, whose binary's SHA-256 is not the pin's, so
//! `xtask witness` must refuse the run before its one witness runs, as it
//! refuses the main oracle's drift.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// A native whose witness runs under the variant.
///
/// ```php
/// var_dump(ctype_digit('7'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn ctype_digit() {}
