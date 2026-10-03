//! A negative control (harness-design.md §4.5 condition 2; design §8):
//! an engine whose natives stand on the attributes (ADR-0007) and whose
//! `ops` module holds one kernel without a witness, which the coverage
//! half of `xtask witness` must fail on, naming it (#1264). The anchor
//! counts the native; the kernels are counted beside it, and the helper
//! below, outside `ops`, is neither.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod ops;

use php_macros::php_function;

/// A native with its own witness.
///
/// ```php
/// var_dump(fixture_strlen('ab'));
/// ```
/// ```expect
/// int(2)
/// ```
#[php_function]
pub fn fixture_strlen(text: String) -> i64 {
    text.len() as i64
}

/// A helper no attribute declares and no `ops` holds, and so no subject.
pub fn helper() {}
