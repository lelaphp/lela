//! The native attributes (design §4.1, §7.1; ADR-0002): `#[php_function]`
//! and `#[php_class]` derive a native's declaration from its Rust
//! signature and emit the entry that takes the engine's concrete
//! `Request`; `#[php_const]` declares a constant; `#[tentative]` marks a
//! method's return type as tentative, an argument the class attribute
//! reads. Each expands to paths in `php_ext`, the one crate every
//! extension names. This skeleton hands every item back unchanged, so
//! the attributes exist for `api.lock`'s `attr` group and a body compiles
//! before the expansion does; the expansion is inherited from Mahout's
//! macros as its own work.
#![forbid(unsafe_code)]

use proc_macro::TokenStream;

/// A native function: its declaration from the signature, its entry
/// over `&mut Request`.
#[proc_macro_attribute]
pub fn php_function(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// A native class: its declaration from the `impl` block, each method's
/// entry, and the `#[tentative]` marks it carries.
#[proc_macro_attribute]
pub fn php_class(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// A constant an extension declares.
#[proc_macro_attribute]
pub fn php_const(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// The tentative bit on a native method's return type (design §4.7):
/// php enforces the type on the native and deprecates, rather than
/// refuses, a user override that drops it.
#[proc_macro_attribute]
pub fn tentative(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    item
}
