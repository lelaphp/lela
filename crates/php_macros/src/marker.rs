//! `#[mirrors("ext/x/php_x.h", "sym")]`: the php-src declaration a
//! surface item stands for (design §4.10, #2020). The item is emitted as
//! written; what the marker says is held by `xtask api-check`, which reads
//! it from the sources against the pinned php-src. Here only its shape is
//! refused: two non-empty string literals, the header and the symbol.

use proc_macro2::{Span, TokenStream as Tokens};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{LitStr, Token};

use crate::error;

/// The item unchanged, once the marker's arguments are a header and a
/// symbol.
pub(crate) fn expand(attribute: Tokens, item: Tokens) -> syn::Result<Tokens> {
    let arguments = Punctuated::<LitStr, Token![,]>::parse_terminated.parse2(attribute)?;
    let named: Vec<String> = arguments.iter().map(LitStr::value).collect();
    match named.as_slice() {
        [header, symbol] if !header.is_empty() && !symbol.is_empty() => Ok(item),
        _ => Err(error(
            arguments.first().map_or_else(Span::call_site, LitStr::span),
            "`#[mirrors]` takes the header, from php-src's root, and the symbol it declares: `#[mirrors(\"ext/random/php_random.h\", \"php_random_default_algo\")]`",
        )),
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::expand;

    #[test]
    fn the_item_passes_through_under_a_header_and_a_symbol() {
        let item = quote!(
            pub fn default_engine() -> Rng {
                Rng
            }
        );
        let expanded = expand(
            quote!("ext/random/php_random.h", "php_random_default_algo"),
            item.clone(),
        )
        .unwrap();
        assert_eq!(expanded.to_string(), item.to_string());
    }

    #[test]
    fn anything_but_two_named_strings_is_refused() {
        for attribute in [
            quote!(),
            quote!("ext/random/php_random.h"),
            quote!("ext/random/php_random.h", ""),
            quote!("a.h", "b", "c"),
            quote!(php_random_default_algo),
        ] {
            assert!(
                expand(
                    attribute.clone(),
                    quote!(
                        fn f() {}
                    )
                )
                .is_err(),
                "{attribute}"
            );
        }
    }
}
