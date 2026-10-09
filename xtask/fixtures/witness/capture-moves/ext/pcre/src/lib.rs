//! A negative control (harness-design.md §4.1, §8): an extension whose one
//! witness is marked `capture` and prints a number its scratch directory's
//! path moves, which `xtask witness --accept --streams` must refuse to
//! write, naming the line, and fail the crate on (#801). `__FILE__` is the
//! oracle's scratch path, which the recipe's normalisation writes as its
//! token, and `string(N)` counts the path before it did, so the oracle's
//! two runs from two scratch directories print two lengths. The fence's
//! body is left as it stands, and so is this file.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// A native whose witness prints what moves with the directory.
///
/// ```php
/// var_dump(__FILE__);
/// ```
/// ```expect capture
/// ```
pub fn ctype_digit() {}
