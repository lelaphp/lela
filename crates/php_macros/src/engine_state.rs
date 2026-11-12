//! `#[derive(EngineState)]` (ADR-0005; #4348 ruling 2): a `struct` or an
//! `enum` is engine state where every field is, at any depth. A field
//! whose type names a type parameter is held to the bound in the impl's
//! `where` clause, so a generic type is engine state for the parameters
//! its fields allow; every other field is held in a function beside the
//! impl, spanned at the field's type, so a type holding a pointer into a
//! request's heap fails to compile at that field, which the error shows.
//! A field written as `fn(..)` is code and is passed unread. The trait is
//! `php_heap`'s, re-exported at the api's `abi::EngineState`, and named
//! at `php_heap` itself in a crate below the engine (#4544).

use proc_macro2::TokenStream as Tokens;
use quote::{quote, quote_spanned};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{Data, DeriveInput, Fields, Ident, Type};

use crate::{error, root_for};

/// The impl, and the function holding each field the `where` clause
/// does not.
pub(crate) fn expand(item: Tokens) -> syn::Result<Tokens> {
    let input: DeriveInput = syn::parse2(item)?;
    let state = state_trait();
    let fields: Vec<&Fields> = match &input.data {
        Data::Struct(data) => vec![&data.fields],
        Data::Enum(data) => data.variants.iter().map(|v| &v.fields).collect(),
        Data::Union(data) => {
            return Err(error(
                data.union_token.span(),
                "`#[derive(EngineState)]` reads a struct's or an enum's fields; a union's are not known to be the one it holds",
            ));
        }
    };
    let params: Vec<Ident> = (input.generics.type_params())
        .map(|param| param.ident.clone())
        .collect();
    let mut generics = input.generics.clone();
    let clause = generics.make_where_clause();
    let mut held = Vec::new();
    for field in fields.iter().flat_map(|fields| fields.iter()) {
        let ty = &field.ty;
        if code(ty) {
            continue;
        }
        if names_any(ty, &params) {
            clause.predicates.push(syn::parse_quote!(#ty: #state));
        } else {
            held.push(quote_spanned!(ty.span()=> held::<#ty>();));
        }
    }
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let name = &input.ident;
    Ok(quote! {
        // Every field is engine state: held to the bound in this `where`
        // clause where its type names a parameter, and below where not.
        impl #impl_generics #state for #name #ty_generics #where_clause {}

        const _: () = {
            #[allow(dead_code, clippy::extra_unused_type_parameters)]
            fn fields #impl_generics () #where_clause {
                fn held<T: ?::core::marker::Sized + #state>() {}
                #(#held)*
            }
        };
    })
}

/// The path `#[derive(EngineState)]` names the trait by: `php_heap`'s own
/// in a crate below the engine, `php_unit` and `php_ssa`, which depend on
/// `php_heap` and on neither the engine nor the facade, and the root's
/// `abi::EngineState` everywhere else (ADR-0007, #4544).
fn state_trait() -> Tokens {
    state_trait_for(std::env::var("CARGO_CRATE_NAME").ok().as_deref())
}

pub(crate) fn state_trait_for(crate_name: Option<&str>) -> Tokens {
    if matches!(crate_name, Some("php_unit" | "php_ssa")) {
        quote!(::php_heap::EngineState)
    } else {
        let root = root_for(crate_name);
        quote!(#root::abi::EngineState)
    }
}

/// Whether `ty` names any of `params`.
fn names_any(ty: &Type, params: &[Ident]) -> bool {
    struct Names<'p> {
        params: &'p [Ident],
        found: bool,
    }
    impl<'ast> Visit<'ast> for Names<'_> {
        fn visit_path(&mut self, path: &'ast syn::Path) {
            if let Some(first) = path.segments.first()
                && self.params.contains(&first.ident)
            {
                self.found = true;
            }
            syn::visit::visit_path(self, path);
        }
    }
    let mut names = Names {
        params,
        found: false,
    };
    names.visit_type(ty);
    names.found
}

/// Whether `ty` is written as a function pointer, `fn(..) -> ..`: code
/// the binary holds, which carries no value.
fn code(ty: &Type) -> bool {
    match ty {
        Type::FnPtr(_) => true,
        Type::Paren(inner) => code(&inner.elem),
        Type::Group(inner) => code(&inner.elem),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::expand;
    use crate::refused;

    #[test]
    fn each_field_but_code_is_held_in_the_where_clause() {
        let expanded = expand(quote!(
            struct Kept<K> {
                key: K,
                names: Vec<Box<[u8]>>,
                read: fn(&[u8]) -> usize,
            }
        ))
        .expect("a struct expands")
        .to_string();
        assert!(
            expanded.contains("K : :: php_ext :: abi :: EngineState"),
            "{expanded}"
        );
        assert!(
            expanded.contains("held :: < Vec < Box < [u8] > > > ()"),
            "{expanded}"
        );
        assert!(!expanded.contains("fn (& [u8]) -> usize"), "{expanded}");
    }

    #[test]
    fn every_variants_fields_are_held() {
        let expanded = expand(quote!(
            enum Loaded {
                Path(std::path::PathBuf),
                Failed { message: String },
                Empty,
            }
        ))
        .expect("an enum expands")
        .to_string();
        assert!(
            expanded.contains("held :: < std :: path :: PathBuf > ()"),
            "{expanded}"
        );
        assert!(expanded.contains("held :: < String > ()"), "{expanded}");
    }

    #[test]
    fn a_union_is_refused() {
        assert_eq!(
            refused(expand(quote!(
                union Word {
                    n: u64,
                    f: f64,
                }
            ))),
            "php_macros: `#[derive(EngineState)]` reads a struct's or an enum's fields; a union's are not known to be the one it holds"
        );
    }
}
