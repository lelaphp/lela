//! A negative control (harness-design.md §4.1, §8; #3455): an extension
//! whose one witness is a plain fence the oracle does not meet and prints
//! a number its scratch directory's path moves, which `xtask witness
//! --accept --bodies` must refuse to write, naming the line, and fail the
//! crate on. `__FILE__` is the oracle's scratch path, which the recipe's
//! normalisation writes as its token, and `string(N)` counts the path
//! before it did, so the oracle's two runs from two scratch directories
//! print two lengths, and the authored body has no wildcard that meets
//! both. The fence's body is left as it stands, and so is this file.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// A native whose witness prints what moves with the directory.
///
/// ```php
/// var_dump(__FILE__);
/// ```
/// ```expect
/// stale
/// ```
pub fn ctype_digit() {}
