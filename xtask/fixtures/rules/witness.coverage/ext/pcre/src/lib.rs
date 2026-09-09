//! A negative-control fixture (harness-design.md §4.1): an extension whose
//! two natives are one with a witness and one without, which is what the
//! coverage half of `xtask witness` must fail on.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// A native with a witness beside it.
///
/// ```php
/// var_dump(ctype_digit('7'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn ctype_digit() {}

/// A native with no witness: the one the rule fails on.
pub fn ctype_alpha() {}
