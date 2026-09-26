//! A negative control (harness-design.md §4.5 condition 2, revision 57):
//! an extension whose natives stand on the attributes, one function
//! without a witness and one method no fence of its class calls, which is
//! what the coverage half of `xtask witness` must fail on, naming both.
//! The helper below them is no native, and is not counted once the
//! attributes are what declares one.
#![forbid(unsafe_code)]
// The `expect` fence is `xtask witness`'s, not rustdoc's (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::attr::{php_class, php_function};

/// A function with its own witness.
///
/// ```php
/// var_dump(preg_quote('.'));
/// ```
/// ```expect
/// string(2) "\."
/// ```
#[php_function]
pub fn preg_quote(text: String) -> String {
    text
}

/// A function with none: the first of the two the rule fails on.
#[php_function]
pub fn preg_last_error() -> i64 {
    0
}

/// The class's header, whose fence calls `test` and not `replace`.
///
/// ```php
/// $p = new Pattern; var_dump($p->test('a'));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_class]
pub struct Pattern;

#[php_class]
impl Pattern {
    fn test(this: This<Self>, subject: String) -> bool {
        !subject.is_empty()
    }

    /// No fence calls it: the second of the two the rule fails on.
    fn replace(this: This<Self>, subject: String) -> String {
        subject
    }
}

/// A helper no attribute declares, and so no native.
pub fn helper() {}
