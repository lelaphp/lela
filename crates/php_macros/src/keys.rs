//! The keys of an attribute's own arguments and of the inert `#[php(..)]`
//! (design §7.1): `name`, `name = tokens` or `name(tokens)`, split at the
//! top-level commas, each held to the keys its item takes, in the form
//! that key is written, with its error at the key's own token.

use proc_macro2::{Delimiter, Span, TokenStream as Tokens, TokenTree};
use syn::spanned::Spanned;
use syn::{Attribute, Ident};

use crate::error;

/// How a key is written.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Form {
    /// `final`
    Flag,
    /// `default = 0`
    Eq,
    /// `namespace(Dom)`
    List,
    /// `payload`, or `payload = T`
    FlagOrEq,
    /// `deprecated`, or `deprecated(since = "8.1")`
    FlagOrList,
}

/// What a key carries after its name.
pub(crate) enum Value {
    Flag,
    Eq(Tokens),
    List(Tokens),
}

pub(crate) struct Key {
    pub(crate) name: Ident,
    pub(crate) value: Value,
}

/// The keys of one attribute or one item's `#[php(..)]`s.
#[derive(Default)]
pub(crate) struct Keys(Vec<Key>);

impl Keys {
    /// The keys of an attribute's own tokens.
    pub(crate) fn parse(tokens: Tokens) -> syn::Result<Keys> {
        let mut keys = Vec::new();
        let mut current: Vec<TokenTree> = Vec::new();
        for tree in tokens {
            match &tree {
                TokenTree::Punct(punct) if punct.as_char() == ',' => {
                    keys.push(key(std::mem::take(&mut current))?);
                }
                _ => current.push(tree),
            }
        }
        if !current.is_empty() {
            keys.push(key(current)?);
        }
        Ok(Keys(keys))
    }

    /// The keys of every `#[php(..)]` on an item, the attributes removed
    /// from it.
    pub(crate) fn take(attrs: &mut Vec<Attribute>) -> syn::Result<Keys> {
        let mut keys = Keys::default();
        let mut kept = Vec::new();
        for attr in attrs.drain(..) {
            if attr.path().is_ident("php") {
                let syn::Meta::List(list) = &attr.meta else {
                    return Err(error(attr.span(), "`#[php(..)]` takes keys"));
                };
                keys.0.extend(Keys::parse(list.tokens.clone())?.0);
            } else {
                kept.push(attr);
            }
        }
        *attrs = kept;
        Ok(keys)
    }

    /// Hold the keys to those `what` takes, each once and in its form.
    pub(crate) fn allow(&self, allowed: &[(&str, Form)], what: &str) -> syn::Result<()> {
        for (at, key) in self.0.iter().enumerate() {
            let Some((_, form)) = allowed.iter().find(|(name, _)| key.name == name) else {
                let names: Vec<&str> = allowed.iter().map(|(name, _)| *name).collect();
                return Err(error(
                    key.name.span(),
                    &format!(
                        "`{}` is not a key of {what}, which takes {}",
                        key.name,
                        names.join(", ")
                    ),
                ));
            };
            // `attribute(..)` is the one key an item may give more than
            // once, one php attribute each.
            if key.name != "attribute"
                && self.0[..at].iter().any(|earlier| earlier.name == key.name)
            {
                return Err(error(
                    key.name.span(),
                    &format!("`{}` is given twice", key.name),
                ));
            }
            let fits = matches!(
                (form, &key.value),
                (Form::Flag | Form::FlagOrEq | Form::FlagOrList, Value::Flag)
                    | (Form::Eq | Form::FlagOrEq, Value::Eq(_))
                    | (Form::List | Form::FlagOrList, Value::List(_))
            );
            if !fits {
                let shape = match form {
                    Form::Flag => "`name`",
                    Form::Eq => "`name = ..`",
                    Form::List => "`name(..)`",
                    Form::FlagOrEq => "`name` or `name = ..`",
                    Form::FlagOrList => "`name` or `name(..)`",
                };
                return Err(error(
                    key.name.span(),
                    &format!("`{}` is written {shape}", key.name),
                ));
            }
        }
        Ok(())
    }

    /// These keys and `other`'s, in that order.
    pub(crate) fn with(mut self, other: Keys) -> Keys {
        self.0.extend(other.0);
        self
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    pub(crate) fn find(&self, name: &str) -> Option<&Key> {
        self.0.iter().find(|key| key.name == name)
    }

    pub(crate) fn eq(&self, name: &str) -> Option<&Tokens> {
        match self.find(name).map(|key| &key.value) {
            Some(Value::Eq(tokens)) => Some(tokens),
            _ => None,
        }
    }

    pub(crate) fn list(&self, name: &str) -> Option<&Tokens> {
        match self.find(name).map(|key| &key.value) {
            Some(Value::List(tokens)) => Some(tokens),
            _ => None,
        }
    }

    /// Every `name(..)` of a key an item may give more than once.
    pub(crate) fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Tokens> + 'a {
        self.0.iter().filter_map(move |key| match &key.value {
            Value::List(tokens) if key.name == name => Some(tokens),
            _ => None,
        })
    }

    /// The span of a key, for an error about it.
    pub(crate) fn span(&self, name: &str) -> Span {
        self.find(name)
            .map_or_else(Span::call_site, |key| key.name.span())
    }
}

fn key(trees: Vec<TokenTree>) -> syn::Result<Key> {
    let mut trees = trees.into_iter();
    let name = match trees.next() {
        Some(TokenTree::Ident(name)) => name,
        Some(other) => return Err(error(other.span(), "a key is an identifier")),
        None => return Err(error(Span::call_site(), "an empty key")),
    };
    let rest: Vec<TokenTree> = trees.collect();
    let value = match rest.split_first() {
        None => Value::Flag,
        Some((TokenTree::Punct(punct), tail)) if punct.as_char() == '=' => {
            if tail.is_empty() {
                return Err(error(punct.span(), "a value follows `=`"));
            }
            Value::Eq(tail.iter().cloned().collect())
        }
        Some((TokenTree::Group(group), [])) if group.delimiter() == Delimiter::Parenthesis => {
            Value::List(group.stream())
        }
        Some((other, _)) => {
            return Err(error(
                other.span(),
                "a key is `name`, `name = ..` or `name(..)`",
            ));
        }
    };
    Ok(Key { name, value })
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::{Form, Keys};
    use crate::refused;

    #[test]
    fn a_key_list_that_does_not_split_into_keys_is_refused() {
        for (tokens, message) in [
            (quote!("x"), "php_macros: a key is an identifier"),
            (quote!(, a), "php_macros: an empty key"),
            (quote!(a,, b), "php_macros: an empty key"),
            (quote!(arity =), "php_macros: a value follows `=`"),
            (
                quote!(arity manual),
                "php_macros: a key is `name`, `name = ..` or `name(..)`",
            ),
            (
                quote!(namespace(Dom) extra),
                "php_macros: a key is `name`, `name = ..` or `name(..)`",
            ),
        ] {
            assert_eq!(refused(Keys::parse(tokens.clone())), message, "{tokens}");
        }
    }

    #[test]
    fn a_php_attribute_without_a_key_list_is_refused() {
        for item in [
            quote!(
                #[php]
                fn f() {}
            ),
            quote!(
                #[php = 1]
                fn f() {}
            ),
        ] {
            let mut function: syn::ItemFn = syn::parse2(item.clone()).expect("the item parses");
            assert_eq!(
                refused(Keys::take(&mut function.attrs)),
                "php_macros: `#[php(..)]` takes keys",
                "{item}"
            );
        }
    }

    #[test]
    fn a_key_the_item_does_not_take_as_written_is_refused() {
        let allowed = [
            ("final", Form::Flag),
            ("arity", Form::Eq),
            ("namespace", Form::List),
            ("payload", Form::FlagOrEq),
            ("deprecated", Form::FlagOrList),
            ("attribute", Form::List),
        ];
        for (tokens, message) in [
            (
                quote!(bogus),
                "php_macros: `bogus` is not a key of a method, which takes final, arity, namespace, payload, deprecated, attribute",
            ),
            (quote!(final, final), "php_macros: `final` is given twice"),
            (quote!(final = 1), "php_macros: `final` is written `name`"),
            (quote!(arity), "php_macros: `arity` is written `name = ..`"),
            (
                quote!(namespace),
                "php_macros: `namespace` is written `name(..)`",
            ),
            (
                quote!(payload(T)),
                "php_macros: `payload` is written `name` or `name = ..`",
            ),
            (
                quote!(deprecated = 1),
                "php_macros: `deprecated` is written `name` or `name(..)`",
            ),
        ] {
            let keys = Keys::parse(tokens.clone()).expect("the keys split");
            assert_eq!(
                refused(keys.allow(&allowed, "a method")),
                message,
                "{tokens}"
            );
        }
        let keys = Keys::parse(quote!(attribute(A), attribute(B))).expect("the keys split");
        assert!(
            keys.allow(&allowed, "a method").is_ok(),
            "`attribute` is given more than once"
        );
    }
}
