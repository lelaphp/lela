//! The native attributes (design §4.1, §7.1; ADR-0007). A native is a
//! Rust item with an attribute on it and nothing else:
//!
//! - `#[php_function]` on a `fn` declares a function. It re-emits the
//!   `fn` and a sibling `mod` of the same name whose `declared()` answers
//!   the declaration and its entry, the pair `ExtensionBuilder::functions`
//!   takes.
//! - `#[php_class]` on a `struct` or an `enum` declares a class header,
//!   and on an `impl` the class's members, registered together by
//!   `ExtensionBuilder::class::<C>()`. A Rust `enum` is a PHP enum.
//! - `#[php_const]` on a `const` or an inline `mod` declares constants.
//!
//! Beside them, `#[mirrors]` on an item of an extension's surface names
//! the php-src declaration it stands for and expands to the item as
//! written; `xtask api-check` holds what it names (design §4.10, #2020).
//!
//! What a Rust type cannot say is written in the inert `#[php(..)]`, which
//! these three read and strip. A parameter's PHP type, by-reference bit,
//! hold and absent path are its Rust type's, through `FromArgument`, and a return's
//! through `IntoReturn`: the expansion matches no type by name. Every PHP
//! name is the Rust identifier with `r#` stripped, and no string carries a
//! signature, a type or a name. A default is php's constant expression,
//! read as php's lexer reads it and kept unevaluated.
//!
//! The expansion names `::php_ext::abi` and `::php_ext::__private`, or
//! `crate::api` while `php_engine` is compiled, so Core's natives take the
//! same attributes (ADR-0007).
//!
//! What fails the build is held by `compile_fail` blocks in
//! `php_bundle`'s `refusals`, and the parity extension that holds
//! the expansion to the oracle is `php_bundle`'s tests: both need the
//! facade, and this crate depends on no workspace crate.
#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as Tokens};
use quote::quote;
use syn::ext::IdentExt;
use syn::{Attribute, Ident};

mod class;
mod constant;
mod expr;
mod function;
mod keys;
mod marker;

/// A native function: its declaration from the signature, its entry as
/// php's ZPP.
#[proc_macro_attribute]
pub fn php_function(attribute: TokenStream, item: TokenStream) -> TokenStream {
    function::expand(attribute.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// A class: on a `struct` or an `enum` its header, on an `impl` its
/// members.
#[proc_macro_attribute]
pub fn php_class(attribute: TokenStream, item: TokenStream) -> TokenStream {
    class::expand(attribute.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Constants: one `const`, or every item of an inline `mod` in order.
#[proc_macro_attribute]
pub fn php_const(attribute: TokenStream, item: TokenStream) -> TokenStream {
    constant::expand(attribute.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// The php-src declaration a surface item stands for: the header from
/// php-src's root and the symbol it declares `PHPAPI`, `PDO_API` or
/// `ZEND_API`. The item is emitted unchanged.
#[proc_macro_attribute]
pub fn mirrors(attribute: TokenStream, item: TokenStream) -> TokenStream {
    marker::expand(attribute.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Where the expansion's paths start: `crate::api` while `php_engine`
/// itself is compiled, whose natives are Core's, and the facade
/// everywhere else (ADR-0007).
fn root() -> Tokens {
    root_for(std::env::var("CARGO_CRATE_NAME").ok().as_deref())
}

fn root_for(crate_name: Option<&str>) -> Tokens {
    if crate_name == Some("php_engine") {
        quote!(crate::api)
    } else {
        quote!(::php_ext)
    }
}

fn error(span: Span, text: &str) -> syn::Error {
    syn::Error::new(span, format!("php_macros: {text}"))
}

/// The PHP name of an identifier: the identifier, `r#` stripped.
fn php_name(ident: &Ident) -> String {
    ident.unraw().to_string()
}

fn bytes(text: &str) -> syn::LitByteStr {
    syn::LitByteStr::new(text.as_bytes(), Span::call_site())
}

/// `Name` of `text`, as the expansion writes one.
fn name(text: &str) -> Tokens {
    let root = root();
    let text = bytes(text);
    quote!(#root::decl::Name(#text.as_slice().into()))
}

/// The item's `#[cfg(..)]`s, which every generated piece carries.
fn cfgs(attrs: &[Attribute]) -> Vec<Attribute> {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("cfg"))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::root_for;

    #[test]
    fn the_crate_being_compiled_picks_the_root() {
        assert_eq!(root_for(Some("php_engine")).to_string(), "crate :: api");
        assert_eq!(root_for(Some("php_ext_ctype")).to_string(), ":: php_ext");
        assert_eq!(root_for(None).to_string(), ":: php_ext");
    }
}
