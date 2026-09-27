//! A negative control (harness-design.md §4.1; #1037, #1043): an extension
//! whose one witness ends the process serving it on `SIGABRT`, which is
//! what `xtask witness` must fail on, with a reading naming the signal and
//! the snippet being served. The snippet sends the signal only under the
//! linked engine, which names it `witness.php`; the oracle runs it by its
//! scratch path and prints nothing, so the oracle's run is whole and the
//! linked engine is served.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// A native whose witness kills the process that serves it. The `sleep`
/// keeps the snippet in flight until the signal lands, which a signal sent
/// to the process does on whichever thread takes it, not always before
/// `exec` returns.
///
/// ```php
/// if (__FILE__ === 'witness.php') { exec('kill -ABRT ' . getmypid()); sleep(60); }
/// ```
/// ```expect
/// ```
pub fn preg_quote() {}
