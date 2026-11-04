//! `#[php_function]`, and the signature walk a method shares with it: the
//! parameters' declaration from their types, the entry's prologue as
//! php's ZPP (the arity, then each argument left to right), and the
//! `const` assertions that refuse what php's compiler refuses.

use proc_macro2::{Ident, TokenStream as Tokens, TokenTree};
use quote::quote;
use syn::ext::IdentExt;
use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::{FnArg, ItemFn, Pat, ReturnType, Type};

use crate::expr::{self, const_expr};
use crate::keys::{Form, Keys};
use crate::{bytes, cfgs, error, php_name, root};

/// One PHP parameter of a native.
pub(crate) struct Param {
    pub(crate) ident: Ident,
    pub(crate) ty: Type,
    default: Option<Default>,
    keys: Keys,
}

/// A native's signature as the attribute reads it.
pub(crate) struct Signature {
    /// Whether the body takes `&mut Request` first.
    pub(crate) request: bool,
    /// The receiver's type, `This<Self>`, where the native is an instance
    /// method.
    pub(crate) this: Option<Type>,
    pub(crate) params: Vec<Param>,
    /// The return type, `()` where none is written.
    pub(crate) output: Type,
}

/// A parameter's `#[php(default = ..)]`.
struct Default {
    /// The `ConstExpr` the declaration carries.
    expr: Tokens,
    /// The `Absent` the entry lays down where the call gives no argument
    /// ([`expr::absent`]).
    absent: Tokens,
    /// Whether the default is `null`.
    null: bool,
}

const PARAMETER_KEYS: &[(&str, Form)] = &[
    ("default", Form::Eq),
    ("sensitive", Form::Flag),
    ("prefer_ref", Form::Flag),
    ("attribute", Form::List),
];

/// Read a native's signature, stripping each parameter's `#[php(..)]`.
pub(crate) fn read(sig: &mut syn::Signature) -> syn::Result<Signature> {
    if !sig.generics.params.is_empty() || sig.asyncness.is_some() || sig.variadic.is_some() {
        return Err(error(
            sig.span(),
            "a native is a plain `fn`: no generics, no `async`, no C variadic",
        ));
    }
    let mut out = Signature {
        request: false,
        this: None,
        params: Vec::new(),
        output: match &sig.output {
            ReturnType::Type(_, ty) => ty.as_ref().clone(),
            ReturnType::Default => syn::parse_quote!(()),
        },
    };
    for input in &mut sig.inputs {
        let FnArg::Typed(typed) = input else {
            return Err(error(
                input.span(),
                "a native takes no Rust `self`; the receiver is `this: This<Self>`",
            ));
        };
        let keys = Keys::take(&mut typed.attrs)?;
        // `_: This<Self>`: the receiver of a body that reads nothing of
        // it, as php's own reads nothing of `ZEND_THIS`. A PHP parameter
        // has a name, so `_` is never one.
        let receiver = match typed.pat.as_ref() {
            Pat::Wild(_) => true,
            Pat::Ident(pat) => pat.ident == "this" || pat.ident == "_this",
            _ => false,
        };
        if receiver && !matches!(typed.ty.as_ref(), Type::Reference(_)) {
            if !out.params.is_empty() {
                return Err(error(typed.span(), "`this` comes first after the request"));
            }
            out.this = Some(typed.ty.as_ref().clone());
            continue;
        }
        let Pat::Ident(pat) = typed.pat.as_ref() else {
            return Err(error(
                typed.pat.span(),
                "a parameter is an identifier, its PHP name",
            ));
        };
        if let Type::Reference(reference) = typed.ty.as_ref()
            && reference.mutability.is_some()
        {
            if out.request || out.this.is_some() || !out.params.is_empty() {
                return Err(error(typed.span(), "`&mut Request` comes first"));
            }
            out.request = true;
            continue;
        }
        keys.allow(PARAMETER_KEYS, "a parameter")?;
        let default = match keys.eq("default") {
            Some(tokens) => {
                let null = matches!(tokens.clone().into_iter().collect::<Vec<_>>().as_slice(),
                    [TokenTree::Ident(word)] if word == "null");
                Some(Default {
                    expr: const_expr(tokens.clone())?,
                    absent: expr::absent(tokens.clone())?,
                    null,
                })
            }
            None => None,
        };
        out.params.push(Param {
            ident: pat.ident.clone(),
            ty: typed.ty.as_ref().clone(),
            default,
            keys,
        });
    }
    Ok(out)
}

/// `Self` in a type spelled as the class it names, for a check that
/// stands outside the impl.
pub(crate) fn unself(ty: &Type, class: Option<&Ident>) -> Tokens {
    fn walk(tokens: Tokens, class: &Ident) -> Tokens {
        tokens
            .into_iter()
            .map(|tree| match tree {
                TokenTree::Ident(ident) if ident == "Self" => TokenTree::Ident(class.clone()),
                TokenTree::Group(group) => {
                    let mut inner =
                        proc_macro2::Group::new(group.delimiter(), walk(group.stream(), class));
                    inner.set_span(group.span());
                    TokenTree::Group(inner)
                }
                other => other,
            })
            .collect()
    }
    class.map_or_else(|| quote!(#ty), |class| walk(quote!(#ty), class))
}

/// The parameters' static entries, `&[..]` of `NativeParameter`s: each
/// one's facts read off its Rust type's `FromArgument` consts and its
/// declared type as that type's `declared`, shared by every native that
/// names the type.
pub(crate) fn declared_parameters(params: &[Param]) -> syn::Result<Tokens> {
    let root = root();
    let mut each = Vec::new();
    for param in params {
        let name = bytes(&php_name(&param.ident));
        let ty = &param.ty;
        let default = param.default.as_ref().map_or_else(
            || quote!(None),
            |default| {
                let expr = &default.expr;
                quote!(Some(|| #expr))
            },
        );
        let prefer_ref = param.keys.has("prefer_ref");
        let attributes = expr::built(&expr::attributes(&param.keys)?);
        each.push(quote! {
            #root::__private::NativeParameter {
                name: #name,
                ty: || <#ty as #root::abi::FromArgument>::DECLARED,
                by_ref: <#ty as #root::abi::FromArgument>::BY_REF,
                prefer_ref: #prefer_ref,
                variadic: <#ty as #root::abi::FromArgument>::VARIADIC,
                passed: <#ty as #root::abi::FromArgument>::PASSED,
                default: #default,
                attributes: #attributes,
            }
        });
    }
    Ok(quote!(&[#(#each),*]))
}

/// The `const` assertions over a signature's types, each evaluated where
/// the item stands: every type's mask (`?mixed`, `bool|false`, a member
/// twice), a variadic only last, and `default = null` only on a type that
/// admits `null`.
pub(crate) fn checks(signature: &Signature, class: Option<&Ident>) -> Vec<Tokens> {
    let root = root();
    let mut out = Vec::new();
    let last = signature.params.len().saturating_sub(1);
    for (at, param) in signature.params.iter().enumerate() {
        let ty = unself(&param.ty, class);
        out.push(quote!(const _: u32 = <#ty as #root::abi::FromArgument>::MASK;));
        if at != last {
            let text = format!("`{}` is variadic and not last", param.ident);
            out.push(quote!(
                const _: () = assert!(!<#ty as #root::abi::FromArgument>::VARIADIC, #text);
            ));
        }
        if param.default.as_ref().is_some_and(|default| default.null) {
            out.push(quote! {
                const _: fn() = || {
                    fn admits<T: #root::abi::NullDefault>() {}
                    admits::<#ty>();
                };
            });
        }
    }
    let output = unself(&signature.output, class);
    out.push(quote!(const _: u32 = <#output as #root::abi::IntoReturn>::MASK;));
    out
}

/// The entry's reads: the native recorded as entered, then what
/// `before_args` names, then the call read once, its arity held and its
/// arguments taken as they stand on it, then each argument held at its
/// position, its default laid down where the call gave none. `scope` names the declaring class for a default
/// that reads `self::`; `site` is the native as the source names it, its
/// `#[php_class]` type or none and its identifier, which `xtask witness`
/// reads a fence's coverage by (harness §4.5 condition 2, revision 58).
pub(crate) fn prologue(
    params: &[Param],
    scope: &Tokens,
    reads: &Reads,
    site: (Option<&Ident>, &Ident),
) -> Tokens {
    let root = root();
    let entered = entered(reads, site);
    let total = params.len();
    let arguments = if reads.manual {
        quote! {
            let __arguments = #root::__private::arguments::<#total>(__request, 0, None)?;
        }
    } else {
        let steps = params.iter().map(|param| {
            let ty = &param.ty;
            let optional = param.default.is_some();
            quote! {
                if #optional
                    || <#ty as #root::abi::FromArgument>::PASSED
                    || <#ty as #root::abi::FromArgument>::VARIADIC
                {
                    break;
                }
                required += 1;
            }
        });
        let unbounded = params.last().map_or(quote!(false), |last| {
            let ty = &last.ty;
            quote!(<#ty as #root::abi::FromArgument>::VARIADIC)
        });
        quote! {
            #[allow(clippy::never_loop, unused_mut)]
            let required = {
                let mut required = 0usize;
                #[allow(unreachable_code)]
                loop {
                    #(#steps)*
                    break;
                }
                required
            };
            let __arguments = #root::__private::arguments::<#total>(
                __request,
                required,
                if #unbounded { None } else { Some(#total) },
            )?;
        }
    };
    let holds = params.iter().enumerate().map(|(at, param)| {
        let ident = &param.ident;
        let ty = &param.ty;
        let position = u32::try_from(at + 1).unwrap_or(u32::MAX);
        let held = held(param, &quote!(__arguments[#at]), scope, position);
        quote!(let #ident: #ty = #held;)
    });
    quote! {
        #entered
        #arguments
        #(#holds)*
    }
}

/// One argument held to its parameter's type at `position`, `given` as
/// the call passed it, its default laid down where the call gave none.
fn held(param: &Param, given: &Tokens, scope: &Tokens, position: u32) -> Tokens {
    let (root, ty) = (root(), &param.ty);
    let absent = param.default.as_ref().map_or_else(
        || quote!(#root::abi::Absent::Value(#root::values::Value::null())),
        |default| default.absent.clone(),
    );
    let absent = quote!(<#ty as #root::abi::FromArgument>::from_default(__request, #absent, #scope, #position)?);
    if given.is_empty() {
        return absent;
    }
    quote! {
        match <#ty as #root::abi::FromArgument>::hold(__request, #given, #position)? {
            Some(held) => held,
            None => #absent,
        }
    }
}

/// One arity of `frameless(..)`: `n`, or `n = path` naming the function
/// that is php's `ZEND_FRAMELESS_FUNCTION` where it answers otherwise
/// than the framed body, called with the native's own parameters.
struct Arity {
    n: usize,
    path: Option<syn::Path>,
}

fn arities(keys: &Keys) -> syn::Result<Vec<Arity>> {
    let Some(tokens) = keys.list("frameless") else {
        return Ok(Vec::new());
    };
    let arity = |input: syn::parse::ParseStream| -> syn::Result<Arity> {
        let n: syn::LitInt = input.parse()?;
        let path = match input.parse::<Option<syn::Token![=]>>()? {
            Some(_) => Some(input.parse()?),
            None => None,
        };
        match n.base10_parse()? {
            n @ 0..=3 => Ok(Arity { n, path }),
            _ => Err(error(
                n.span(),
                "a frameless arity is 0 to 3, as php's ZEND_FRAMELESS_ICALL_n",
            )),
        }
    };
    let list = |input: syn::parse::ParseStream| {
        syn::punctuated::Punctuated::<Arity, syn::Token![,]>::parse_terminated_with(input, arity)
    };
    Ok(list.parse2(tokens.clone())?.into_iter().collect())
}

/// A frameless arity's entry, php's `zflf_*` beside its `zif_*`
/// (design §7.1): the native's own parameters held off the arity's
/// operands by position, no call record read and no count checked, a
/// variadic tail of the operands past its place held where they stand
/// (`FromArgument::tail`), and every parameter past the arity its
/// default; then the body, or the arity's own path.
fn frameless_entry(
    signature: &Signature,
    ident: &Ident,
    arity: &Arity,
    entered: &Tokens,
) -> (Tokens, Tokens) {
    let (root, n) = (root(), arity.n);
    let last = signature.params.len().saturating_sub(1);
    let holds = signature.params.iter().enumerate().map(|(at, param)| {
        let (name, ty) = (&param.ident, &param.ty);
        let position = u32::try_from(at + 1).unwrap_or(u32::MAX);
        let given = if at < n { quote!(__operands[#at]) } else { Tokens::new() };
        let mut held = held(param, &given, &quote!(None), position);
        if at == last {
            let from = at.min(n);
            held = quote! {
                if <#ty as #root::abi::FromArgument>::VARIADIC {
                    <#ty as #root::abi::FromArgument>::tail(__request, &__operands[#from..], #position)?
                } else { #held }
            };
        }
        quote!(let #name: #ty = #held;)
    });
    let callee = arity.path.clone().unwrap_or_else(|| ident.clone().into());
    let request = signature.request.then(|| quote!(__request,));
    let arguments = signature.params.iter().map(|param| &param.ident);
    let entry = quote::format_ident!("frameless_{}", n);
    let variant = quote::format_ident!("{}", ["Zero", "One", "Two", "Three"][n]);
    let function = quote! {
        fn #entry(__request: &mut #root::request::Request, __operands: &[#root::values::Value; #n]) -> Result<#root::values::Owner<#root::values::Value>, #root::raising::Thrown> {
            #entered
            #(#holds)*
            #root::abi::IntoReturn::into_return(super::#callee(#request #(#arguments),*), __request)
        }
    };
    (
        quote!(#root::__private::Frameless::#variant(#entry)),
        function,
    )
}

/// The native recorded as entered, then what `before_args` names.
fn entered(reads: &Reads, (owner, name): (Option<&Ident>, &Ident)) -> Tokens {
    let root = root();
    let before = reads.before.as_ref().map(|path| quote!(#path(__request);));
    let owner = owner.map_or_else(
        || quote!(None),
        |owner| {
            let owner = owner.unraw().to_string();
            quote!(Some(#owner))
        },
    );
    let name = name.unraw().to_string();
    quote! {
        #root::__private::entered(__request, ::core::env!("CARGO_CRATE_NAME"), #owner, #name);
        #before
    }
}

const FUNCTION_KEYS: &[(&str, Form)] = &[
    ("namespace", Form::List),
    ("aliases", Form::List),
    ("frameless", Form::List),
    ("arity", Form::Eq),
    ("before_args", Form::Eq),
    ("deprecated", Form::FlagOrList),
    ("no_discard", Form::FlagOrList),
    ("attribute", Form::List),
];

/// What the attribute says of the entry's reads.
pub(crate) struct Reads {
    /// `arity = manual`: the body counts its own arguments.
    manual: bool,
    /// `before_args = path`: a `fn(&mut Request)` the entry calls before
    /// the arity and the arguments are read, where php's native does its
    /// own work ahead of `zend_parse_parameters`, as ext/intl's
    /// `INTL_METHOD_INIT_VARS` resets the request's error first, so an
    /// argument's `TypeError` leaves no earlier failure standing.
    before: Option<syn::Path>,
}

impl Reads {
    pub(crate) fn of(keys: &Keys) -> syn::Result<Reads> {
        let manual = match keys.eq("arity") {
            None => false,
            Some(tokens) if tokens.to_string() == "manual" => true,
            Some(_) => return Err(error(keys.span("arity"), "`arity` is `arity = manual`")),
        };
        let before = keys
            .eq("before_args")
            .map(|tokens| syn::parse2::<syn::Path>(tokens.clone()))
            .transpose()?;
        Ok(Reads { manual, before })
    }
}

pub(crate) fn expand(attribute: Tokens, item: Tokens) -> syn::Result<Tokens> {
    let keys = Keys::parse(attribute)?;
    if keys.has("tentative") {
        return Err(error(
            keys.span("tentative"),
            "a tentative return is a method's, never a function's (design §4.7)",
        ));
    }
    keys.allow(FUNCTION_KEYS, "`#[php_function]`")?;
    let mut function: ItemFn = syn::parse2(item)?;
    let signature = read(&mut function.sig)?;
    if signature.this.is_some() {
        return Err(error(function.sig.span(), "a function has no receiver"));
    }
    let root = root();
    let ident = function.sig.ident.clone();
    let reads = Reads::of(&keys)?;
    let parameters = declared_parameters(&signature.params)?;
    let prologue = prologue(&signature.params, &quote!(None), &reads, (None, &ident));
    let output = &signature.output;
    let arguments: Vec<&Ident> = signature.params.iter().map(|param| &param.ident).collect();
    let request = signature.request.then(|| quote!(__request,));
    let entered = entered(&reads, (None, &ident));
    let (frameless, frameless_entries): (Vec<Tokens>, Vec<Tokens>) = arities(&keys)?
        .iter()
        .map(|arity| frameless_entry(&signature, &ident, arity, &entered))
        .unzip();
    let attributes = expr::attributes(&keys)?;
    let cfg = cfgs(&function.attrs);
    let checks: Vec<Tokens> = checks(&signature, None)
        .into_iter()
        .map(|check| quote!(#(#cfg)* #check))
        .collect();
    let attributes = expr::built(&attributes);
    // php registers an alias as a `ZEND_RAW_FENTRY` with no frameless
    // infos (`join`, `strchr`), so the native's arities are its own name's.
    let module = |module: &Ident, php: &str, (frameless, entries): (&[Tokens], &[Tokens])| {
        let php = bytes(php);
        quote! {
            #(#cfg)*
            #[allow(non_snake_case)]
            pub(crate) mod #module {
                #[allow(unused_imports)]
                use super::*;
                /// The static entry, what `ExtensionBuilder::functions`
                /// takes.
                pub(crate) const DECLARED: #root::__private::NativeFunction =
                    #root::__private::NativeFunction {
                        name: #php,
                        parameters: #parameters,
                        return_type: || <#output as #root::abi::IntoReturn>::DECLARED,
                        frameless: &[#(#frameless),*],
                        attributes: #attributes,
                        entry,
                    };
                fn entry(__request: &mut #root::request::Request) -> Result<#root::values::Owner<#root::values::Value>, #root::raising::Thrown> {
                    #prologue
                    #root::abi::IntoReturn::into_return(super::#ident(#request #(#arguments),*), __request)
                }
                #(#entries)*
            }
        }
    };
    let own = (&frameless[..], &frameless_entries[..]);
    let mut modules = vec![module(&ident, &expr::qualified(&keys, &ident)?, own)];
    if let Some(tokens) = keys.list("aliases") {
        let aliases = syn::punctuated::Punctuated::<Ident, syn::Token![,]>::parse_terminated
            .parse2(tokens.clone())?;
        for alias in aliases {
            modules.push(module(&alias, &expr::qualified(&keys, &alias)?, (&[], &[])));
        }
    }
    Ok(quote! {
        #[allow(non_snake_case, clippy::too_many_arguments, clippy::needless_pass_by_value)]
        #function
        #(#checks)*
        #(#modules)*
    })
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::expand;
    use crate::refused;

    #[test]
    fn a_function_the_attribute_cannot_declare_is_refused() {
        for (attribute, item, message) in [
            (
                quote!(),
                quote!(
                    fn f<T>() {}
                ),
                "php_macros: a native is a plain `fn`: no generics, no `async`, no C variadic",
            ),
            (
                quote!(),
                quote!(
                    async fn f() {}
                ),
                "php_macros: a native is a plain `fn`: no generics, no `async`, no C variadic",
            ),
            (
                quote!(),
                quote!(
                    fn f(self) {}
                ),
                "php_macros: a native takes no Rust `self`; the receiver is `this: This<Self>`",
            ),
            (
                quote!(),
                quote!(
                    fn f(x: i64, this: This<Self>) {}
                ),
                "php_macros: `this` comes first after the request",
            ),
            (
                quote!(),
                quote!(
                    fn f((a, b): (i64, i64)) {}
                ),
                "php_macros: a parameter is an identifier, its PHP name",
            ),
            (
                quote!(),
                quote!(
                    fn f(x: i64, request: &mut Request) {}
                ),
                "php_macros: `&mut Request` comes first",
            ),
            (
                quote!(arity = auto),
                quote!(
                    fn f() {}
                ),
                "php_macros: `arity` is `arity = manual`",
            ),
            (
                quote!(frameless(4)),
                quote!(
                    fn f() {}
                ),
                "php_macros: a frameless arity is 0 to 3, as php's ZEND_FRAMELESS_ICALL_n",
            ),
            (
                quote!(tentative),
                quote!(
                    fn f() -> i64 {
                        0
                    }
                ),
                "php_macros: a tentative return is a method's, never a function's (design §4.7)",
            ),
            (
                quote!(),
                quote!(
                    fn f(this: This<Self>) {}
                ),
                "php_macros: a function has no receiver",
            ),
        ] {
            assert_eq!(refused(expand(attribute, item.clone())), message, "{item}");
        }
    }
}
