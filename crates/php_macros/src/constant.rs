//! `#[php_const]` (design §7.1): one `const`, which gains a sibling `mod`
//! of its name whose `const DECLARED` is its static entry, or an inline
//! `mod` whose every `const` is an entry in order, gaining `const
//! DECLARED` for the list, as php's `register_*_symbols` lists them. A
//! `fn NAME() -> T` in the `mod` is a constant computed once, as the
//! extension declares it. Each is declared by value, as php declares an
//! extension's constants, typed through `IntoConst`, so a body reads the
//! value the declaration holds.

use proc_macro2::{Ident, TokenStream as Tokens};
use quote::quote;
use syn::Item;
use syn::spanned::Spanned;

use crate::expr;
use crate::keys::{Form, Keys};
use crate::{bytes, cfgs, error, php_name, root};

const HEADER_KEYS: &[(&str, Form)] = &[
    ("namespace", Form::List),
    ("deprecated", Form::FlagOrList),
    ("attribute", Form::List),
];

const ITEM_KEYS: &[(&str, Form)] = &[("deprecated", Form::FlagOrList), ("attribute", Form::List)];

/// The static entry of one constant: its name, the function that
/// builds its value and its attributes'.
fn declaration(
    ident: &Ident,
    keys: &Keys,
    namespace: &[String],
    value: &Tokens,
) -> syn::Result<Tokens> {
    let root = root();
    let mut path = namespace.to_vec();
    path.push(php_name(ident));
    let php = bytes(&path.join("\\"));
    let attributes = expr::built(&expr::attributes(keys)?);
    Ok(quote!(#root::__private::NativeConst {
        name: #php,
        value: || #value,
        attributes: #attributes,
    }))
}

pub(crate) fn expand(attribute: Tokens, item: Tokens) -> syn::Result<Tokens> {
    let root = root();
    let header = Keys::parse(attribute)?;
    header.allow(HEADER_KEYS, "`#[php_const]`")?;
    let namespace = expr::namespace(&header)?;
    match syn::parse2::<Item>(item)? {
        Item::Const(mut constant) => {
            // The attribute's own keys and the item's `#[php(..)]` are
            // one list here, each key once.
            let keys = header.with(Keys::take(&mut constant.attrs)?);
            keys.allow(HEADER_KEYS, "`#[php_const]` on a `const`")?;
            let ident = constant.ident.clone();
            let ty = &constant.ty;
            let cfg = cfgs(&constant.attrs);
            let value = quote!(<#ty as #root::abi::IntoConst>::value(&super::#ident));
            let declaration = declaration(&ident, &keys, &namespace, &value)?;
            Ok(quote! {
                #[allow(non_upper_case_globals)]
                #constant
                #(#cfg)*
                #[allow(non_snake_case)]
                pub(crate) mod #ident {
                    /// The constant's static entry, for `ExtensionBuilder::constants`.
                    pub(crate) const DECLARED: #root::__private::NativeConst = #declaration;
                }
            })
        }
        Item::Mod(mut module) => {
            if header.has("deprecated") {
                return Err(error(
                    header.span("deprecated"),
                    "a constant module's constants each carry their own `deprecated`",
                ));
            }
            let Some((_, items)) = module.content.as_mut() else {
                return Err(error(
                    module.span(),
                    "`#[php_const]` is on an inline module",
                ));
            };
            let mut declarations = Vec::new();
            for item in items.iter_mut() {
                match item {
                    Item::Const(constant) => {
                        let keys = Keys::take(&mut constant.attrs)?;
                        keys.allow(ITEM_KEYS, "a constant")?;
                        let (ident, ty) = (&constant.ident, &constant.ty);
                        let cfg = cfgs(&constant.attrs);
                        let value = quote!(<#ty as #root::abi::IntoConst>::value(&#ident));
                        let declaration = declaration(ident, &keys, &namespace, &value)?;
                        declarations.push(quote!(#(#cfg)* #declaration));
                    }
                    Item::Fn(function) => {
                        let keys = Keys::take(&mut function.attrs)?;
                        keys.allow(ITEM_KEYS, "a constant")?;
                        if !function.sig.inputs.is_empty() {
                            return Err(error(
                                function.sig.inputs.span(),
                                "a computed constant takes nothing",
                            ));
                        }
                        let ident = &function.sig.ident;
                        let cfg = cfgs(&function.attrs);
                        let value = quote!(#root::abi::IntoConst::value(&#ident()));
                        let declaration = declaration(ident, &keys, &namespace, &value)?;
                        declarations.push(quote!(#(#cfg)* #declaration));
                    }
                    Item::Use(_) => {}
                    other => {
                        return Err(error(
                            other.span(),
                            "a constant module holds `const` items, computed `fn`s and `use`s",
                        ));
                    }
                }
            }
            items.push(syn::parse_quote! {
                /// The constants' static entries in order, for
                /// `ExtensionBuilder::constants`.
                pub(crate) const DECLARED: &[#root::__private::NativeConst] =
                    &[#(#declarations),*];
            });
            module
                .attrs
                .push(syn::parse_quote!(#[allow(non_upper_case_globals, non_snake_case)]));
            Ok(quote!(#module))
        }
        other => Err(error(
            other.span(),
            "`#[php_const]` is on a `const` or an inline `mod`",
        )),
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::expand;
    use crate::refused;

    #[test]
    fn what_is_no_constant_or_constant_module_is_refused() {
        for (attribute, item, message) in [
            (
                quote!(deprecated),
                quote!(
                    mod m {}
                ),
                "php_macros: a constant module's constants each carry their own `deprecated`",
            ),
            (
                quote!(),
                quote!(
                    mod m;
                ),
                "php_macros: `#[php_const]` is on an inline module",
            ),
            (
                quote!(),
                quote!(
                    mod m {
                        fn F(x: i64) -> i64 {
                            x
                        }
                    }
                ),
                "php_macros: a computed constant takes nothing",
            ),
            (
                quote!(),
                quote!(
                    mod m {
                        struct S;
                    }
                ),
                "php_macros: a constant module holds `const` items, computed `fn`s and `use`s",
            ),
            (
                quote!(),
                quote!(
                    fn f() {}
                ),
                "php_macros: `#[php_const]` is on a `const` or an inline `mod`",
            ),
        ] {
            assert_eq!(refused(expand(attribute, item.clone())), message, "{item}");
        }
    }
}
