//! A negative control (harness-design.md §4.1, §8): an extension whose one
//! witness names a regime the pin does not carry, which is what `xtask
//! witness` must fail on. A fence names one `[regime.<name>]` of
//! `oracle.toml` and the values are the pin's, written by a person, so a
//! name nothing in the pin carries is a block the lane cannot run and the
//! failure says which name and which regimes the pin has (#610).
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// A native whose witness names a regime nobody wrote.
///
/// ```php
/// var_dump(ctype_digit('7'));
/// ```
/// ```expect regime=no-such-regime
/// bool(true)
/// ```
pub fn ctype_digit() {}
