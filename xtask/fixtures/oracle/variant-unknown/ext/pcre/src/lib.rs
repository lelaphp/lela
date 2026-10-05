//! A negative control (harness-design.md §4.1, §8; #1890): an extension
//! whose `[[crate]]` row names an oracle variant the pin carries no
//! `[variant.<name>]` for, so `xtask witness` and `xtask crosscheck` must
//! fail its readings, naming the variant, rather than read it under the
//! main oracle or under none.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// A native whose witness has no oracle to run under.
///
/// ```php
/// var_dump(ctype_digit('7'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn ctype_digit() {}
