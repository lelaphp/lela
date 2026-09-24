//! A negative control (harness-design.md §4.1, §8): an extension whose one
//! witness pins a stream the oracle does not print, which is what `xtask
//! witness` must fail on, naming the oracle's run. What a fence pins is the
//! oracle's bytes, so the oracle's run is held to it as the engine's is
//! (#783). The fence pins `__FILE__` as the linked engine names the
//! snippet, `witness.php`; the oracle runs it by its scratch path, so its
//! run misses the fence whatever the engine writes.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// A native whose witness pins what the oracle does not print.
///
/// ```php
/// echo __FILE__;
/// ```
/// ```expect
/// witness.php
/// ```
pub fn ctype_digit() {}
