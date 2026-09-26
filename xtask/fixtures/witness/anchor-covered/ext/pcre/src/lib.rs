//! The positive control beside `anchor-missing` (harness-design.md §4.5
//! condition 2, revision 57): an extension whose natives stand on the
//! attributes, every function with its own witness and every method with
//! a body called by a fence of its class, on the header or on the member
//! impl. The body-less alias is no native.
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

/// The class's header, whose fence calls `test`.
///
/// ```php
/// $p = new Pattern; var_dump($p->test('a'));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_class]
pub struct Pattern;

/// The members, whose fence calls `compile`.
///
/// ```php
/// var_dump(Pattern::compile('/a/') instanceof Pattern);
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_class]
impl Pattern {
    fn test(this: This<Self>, subject: String) -> bool {
        !subject.is_empty()
    }

    fn compile(pattern: String) -> Instance<Pattern> {
        Instance::of(Pattern)
    }

    #[php(implementation = Pattern::test)]
    fn matches(this: This<Self>, subject: String) -> bool;
}

/// A helper no attribute declares, and so no native.
pub fn helper() {}
