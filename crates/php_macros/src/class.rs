//! `#[php_class]`: on a `struct` or an `enum` the class header, the
//! `Class` and `Header` a class registers by; on an `impl` the members,
//! its `Members`, with one entry per method that has a body (design §7.1).
//!
//! The member impl cannot see its header, so the header lays down two
//! facts beside the type, `__PHP_ABSTRACT` (an interface or an abstract
//! class, where a body-less method is abstract) and `__PHP_EXTERN` (a
//! class declared elsewhere, where a method is a trampoline), and the
//! member impl asserts on them.

use proc_macro2::{Ident, TokenStream as Tokens};
use quote::{ToTokens, format_ident, quote};
use syn::ext::IdentExt;
use syn::parse::{ParseStream, Parser};
use syn::spanned::Spanned;
use syn::{Attribute, ImplItem, Item, ItemEnum, ItemImpl, ItemStruct, Type};

use crate::expr::{self, const_expr};
use crate::function::{self, Signature, unself};
use crate::keys::{Form, Keys};
use crate::{bytes, cfgs, error, name, php_name, root};

pub(crate) fn expand(attribute: Tokens, item: Tokens) -> syn::Result<Tokens> {
    match syn::parse2::<Item>(item)? {
        Item::Struct(item) => header(attribute, &item),
        Item::Enum(item) => enumeration(attribute, item),
        Item::Impl(item) => members(attribute, item),
        other => Err(error(
            other.span(),
            "`#[php_class]` is on a struct, an enum or an impl",
        )),
    }
}

fn paths(tokens: Option<&Tokens>) -> syn::Result<Vec<syn::Path>> {
    match tokens {
        Some(tokens) => Ok(
            syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated
                .parse2(tokens.clone())?
                .into_iter()
                .collect(),
        ),
        None => Ok(Vec::new()),
    }
}

/// `<P as Class>::NAME` for each class path.
fn class_names(paths: &[syn::Path]) -> Vec<Tokens> {
    let root = root();
    paths
        .iter()
        .map(|path| quote!(<#path as #root::abi::Class>::NAME))
        .collect()
}

/// `impl Class`, and the two facts the member impl asserts on.
fn class_impl(
    ident: &Ident,
    php: &str,
    payload: &Tokens,
    new: &Tokens,
    facts: (bool, bool),
) -> Tokens {
    let root = root();
    let name = bytes(php);
    let (is_abstract, is_extern) = facts;
    quote! {
        impl #root::abi::Class for #ident {
            const NAME: &'static [u8] = #name;
            type Payload = #payload;
            fn new_payload(class: #root::abi::ClassRef) -> Self::Payload {
                let _ = class;
                #new
            }
        }
        impl #ident {
            #[doc(hidden)]
            pub(crate) const __PHP_ABSTRACT: bool = #is_abstract;
            #[doc(hidden)]
            pub(crate) const __PHP_EXTERN: bool = #is_extern;
        }
    }
}

const HEADER_KEYS: &[(&str, Form)] = &[
    ("namespace", Form::List),
    ("extends", Form::List),
    ("implements", Form::List),
    ("interface", Form::Flag),
    ("implemented_by", Form::List),
    ("alias", Form::Eq),
    ("abstract", Form::Flag),
    ("final", Form::Flag),
    ("readonly", Form::Flag),
    ("extern", Form::Flag),
    ("not_serializable", Form::Flag),
    ("use_guards", Form::Flag),
    ("payload", Form::FlagOrEq),
    ("size", Form::Eq),
    ("beside", Form::List),
    ("handlers", Form::Eq),
    ("new", Form::Eq),
    ("dynamic_properties", Form::Eq),
    ("deprecated", Form::FlagOrList),
    ("attribute_class", Form::FlagOrList),
    ("attribute", Form::List),
];

/// The payload type `This<Self>` reaches and how `new` lays it down: the
/// header's own (`payload`, `payload = T`, `new = PATH`), the parent's,
/// or none. The third is whether the class lays its own down.
fn payload(
    keys: &Keys,
    ident: &Ident,
    parent: Option<&syn::Path>,
) -> syn::Result<(Tokens, Tokens, bool)> {
    let root = root();
    let own = keys.find("payload").is_some();
    let payload = match (keys.eq("payload"), parent) {
        (Some(ty), _) => ty.clone(),
        (None, _) if own => quote!(#ident),
        (None, Some(parent)) => quote!(<#parent as #root::abi::Class>::Payload),
        (None, None) => quote!(()),
    };
    let new = match (own, keys.eq("new"), parent) {
        (true, Some(path), _) => quote!((#path)(class)),
        (true, None, _) => quote!(<#payload as ::core::default::Default>::default()),
        (false, Some(_), _) => {
            return Err(error(
                keys.span("new"),
                "`new` lays a payload down; the header names none",
            ));
        }
        (false, None, Some(parent)) => quote!(<#parent as #root::abi::Class>::new_payload(class)),
        (false, None, None) => quote!(),
    };
    Ok((payload, new, own))
}

/// `impl Header` for a class another extension or Core declares: no
/// header of its own, so its member impl's constants go to
/// `class_constants` and its methods to `trampolines`.
fn extern_header(ident: &Ident) -> Tokens {
    let root = root();
    quote! {
        impl #root::abi::Header for #ident {
            const HEADER: Option<&'static #root::__private::NativeClass> = None;
        }
    }
}

/// The header's `ClassFlags` and its attributes as a static entry holds
/// them.
fn flags(keys: &Keys) -> syn::Result<(Tokens, Tokens)> {
    let root = root();
    let (is_abstract, is_final, readonly) = (
        keys.has("abstract"),
        keys.has("final"),
        keys.has("readonly"),
    );
    let not_serializable = keys.has("not_serializable");
    let use_guards = keys.has("use_guards");
    let dynamic = keys.eq("dynamic_properties").map(ToString::to_string);
    let variant = match dynamic.as_deref() {
        None | Some("deprecated") => quote!(Deprecated),
        Some("allowed") => quote!(Allowed),
        Some("forbidden") => quote!(Forbidden),
        Some(_) => {
            return Err(error(
                keys.span("dynamic_properties"),
                "`dynamic_properties` is `forbidden`, `deprecated` or `allowed`",
            ));
        }
    };
    let mut attributes = expr::attributes(keys)?;
    if dynamic.as_deref() == Some("allowed") {
        let php = bytes("AllowDynamicProperties");
        attributes.insert(0, quote!(#root::__private::attribute(#php, &[])));
    }
    let flags = class_flags(
        &[
            ("is_abstract", is_abstract),
            ("is_final", is_final),
            ("is_readonly", readonly),
            ("not_serializable", not_serializable),
            ("use_guards", use_guards),
        ],
        &variant,
    );
    Ok((flags, expr::built(&attributes)))
}

/// `ClassFlags` with `set` true and `dynamic_properties` the variant
/// named, every other bit false, as `ClassFlags::default()` leaves it.
fn class_flags(set: &[(&str, bool)], dynamic: &Tokens) -> Tokens {
    let root = root();
    let bit = |name: &str| set.iter().any(|(bit, on)| *bit == name && *on);
    let (is_abstract, is_final, is_readonly) =
        (bit("is_abstract"), bit("is_final"), bit("is_readonly"));
    let (not_serializable, use_guards) = (bit("not_serializable"), bit("use_guards"));
    quote!(#root::decl::ClassFlags {
        is_abstract: #is_abstract,
        is_final: #is_final,
        is_readonly: #is_readonly,
        is_anonymous: false,
        is_top_level: false,
        not_serializable: #not_serializable,
        dynamic_properties: #root::decl::DynamicProperties::#dynamic,
        use_guards: #use_guards,
        hooked_variance: false,
        stringable_added: false,
    })
}

/// The pieces of one `NativeClass` the header and the enum header
/// differ in.
struct Entry {
    name: syn::LitByteStr,
    kind: Tokens,
    flags: Tokens,
    parent: Tokens,
    interfaces: Vec<Tokens>,
    implementors: Vec<Tokens>,
    aliases: Vec<Tokens>,
    backing: Tokens,
    constants: Vec<Tokens>,
    attributes: Tokens,
    object_size: Tokens,
    object_beside: Tokens,
    objects: Tokens,
}

/// `const HEADER` of a class with a header of its own.
fn header_impl(ident: &Ident, entry: &Entry) -> Tokens {
    let root = root();
    let Entry {
        name,
        kind,
        flags,
        parent,
        interfaces,
        implementors,
        aliases,
        backing,
        constants,
        attributes,
        object_size,
        object_beside,
        objects,
    } = entry;
    quote! {
        impl #root::abi::Header for #ident {
            const HEADER: Option<&'static #root::__private::NativeClass> =
                Some(&#root::__private::NativeClass {
                    name: #name,
                    kind: #root::decl::ClassKind::#kind,
                    flags: #flags,
                    parent: #parent,
                    interfaces: &[#(#root::decl::Name::borrowed(#interfaces)),*],
                    implementors: &[#(#root::decl::Name::borrowed(#implementors)),*],
                    aliases: &[#(#root::decl::Name::borrowed(#aliases)),*],
                    backing: #root::decl::EnumBacking::#backing,
                    constants: &[#(#constants),*],
                    attributes: #attributes,
                    object_size: #object_size,
                    object_beside: &[#object_beside],
                    objects: #objects,
                });
        }
    }
}

fn header(attribute: Tokens, item: &ItemStruct) -> syn::Result<Tokens> {
    let root = root();
    let keys = Keys::parse(attribute)?;
    keys.allow(HEADER_KEYS, "a class header")?;
    if !item.generics.params.is_empty() {
        return Err(error(
            item.generics.span(),
            "a class header has no generics",
        ));
    }
    let ident = &item.ident;
    let php = expr::qualified(&keys, ident)?;
    let interface = keys.has("interface");
    let extends = paths(keys.list("extends"))?;
    if !interface && extends.len() > 1 {
        return Err(error(keys.span("extends"), "a class extends one parent"));
    }
    let parent = extends.first().filter(|_| !interface);
    let (payload, new, own) = payload(&keys, ident, parent)?;
    let facts = (interface || keys.has("abstract"), keys.has("extern"));
    let mut class = class_impl(ident, &php, &payload, &new, facts);
    // `size = N`, php's `sizeof` of the struct `create_object` asks
    // `zend_object_alloc` for: the payload sits in the bytes it lays
    // beside the `zend_object`, so it must fit them (design §5.13).
    // `beside(..)`, the `emalloc`s the same `create_object` makes, is
    // the struct's company and names no struct of its own.
    if keys.list("beside").is_some() && keys.eq("size").is_none() {
        return Err(error(
            keys.span("beside"),
            "`beside(..)` goes with the `size = N` of the struct it is allocated beside",
        ));
    }
    if let Some(size) = keys.eq("size") {
        let message = format!("the payload of `{php}` does not fit the struct php allocates");
        class.extend(quote! {
            const _: () = ::core::assert!(
                ::core::mem::size_of::<#payload>() + 56 <= #size
                    && ::core::mem::align_of::<#payload>() <= 8,
                #message
            );
        });
    }
    let allow = quote!(#[allow(non_camel_case_types, clippy::upper_case_acronyms)]);
    let name = bytes(&php);
    if keys.has("extern") {
        let header = extern_header(ident);
        return Ok(quote!(#allow #item #class #header));
    }
    let kind = if interface {
        quote!(Interface)
    } else {
        quote!(Class)
    };
    let parent = parent.map_or_else(
        || quote!(None),
        |parent| quote!(Some(<#parent as #root::abi::Class>::NAME)),
    );
    let mut interfaces = if interface {
        class_names(&extends)
    } else {
        Vec::new()
    };
    interfaces.extend(class_names(&paths(keys.list("implements"))?));
    let implementors = class_names(&paths(keys.list("implemented_by"))?);
    let aliases = match keys.eq("alias") {
        Some(tokens) => {
            vec![bytes(&syn::parse2::<syn::LitStr>(tokens.clone())?.value()).into_token_stream()]
        }
        None => Vec::new(),
    };
    let (flags, attributes) = flags(&keys)?;
    let handlers = keys.eq("handlers").map(|handlers| quote!(&#handlers));
    let objects = match (own, handlers) {
        (true, handlers) => {
            let handlers = handlers.unwrap_or_else(|| quote!(&#root::abi::Std));
            quote!(Some((#handlers, Some(#root::__private::create_object::<Self>))))
        }
        (false, Some(handlers)) => quote!(Some((#handlers, None))),
        (false, None) => quote!(None),
    };
    let header = header_impl(
        ident,
        &Entry {
            name,
            kind,
            flags,
            parent,
            interfaces,
            implementors,
            aliases,
            backing: quote!(Pure),
            constants: Vec::new(),
            attributes,
            object_size: keys.eq("size").cloned().unwrap_or_else(|| quote!(0)),
            object_beside: keys.list("beside").cloned().unwrap_or_default(),
            objects,
        },
    );
    Ok(quote! {
        #allow
        #item
        #class
        #header
    })
}

const ENUM_KEYS: &[(&str, Form)] = &[
    ("namespace", Form::List),
    ("backed", Form::Eq),
    ("implements", Form::List),
    ("deprecated", Form::FlagOrList),
    ("attribute", Form::List),
];

/// What an enum's cases are backed by.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Backing {
    Pure,
    String,
    Int,
}

/// Each case as `(php name, variant)` for `Cases`, and as the class
/// constant a case is.
fn cases(
    item: &mut ItemEnum,
    backing: Backing,
    php: &str,
) -> syn::Result<(Vec<Tokens>, Vec<Tokens>)> {
    let root = root();
    let ident = item.ident.clone();
    let mut table = Vec::new();
    let mut cases = Vec::new();
    for variant in &mut item.variants {
        let keys = Keys::take(&mut variant.attrs)?;
        keys.allow(&[("value", Form::Eq)], "an enum case")?;
        if !matches!(variant.fields, syn::Fields::Unit) {
            return Err(error(variant.span(), "an enum case carries no fields"));
        }
        let case = bytes(&php_name(&variant.ident));
        let v = &variant.ident;
        table.push(quote!((#case.as_slice(), #ident::#v)));
        let value = match (backing, keys.eq("value")) {
            (Backing::String, Some(tokens)) => const_expr(tokens.clone())?,
            (Backing::String, None) => {
                return Err(error(
                    variant.span(),
                    "a string-backed case takes `#[php(value = \"..\")]`",
                ));
            }
            (Backing::Int, None) => quote!(#root::decl::ConstExpr::long(#ident::#v as i64)),
            (Backing::Pure, None) => {
                let (class, case) = (name(php), name(&php_name(&variant.ident)));
                quote!(#root::decl::ConstExpr::EnumCase { class: #class, case: #case, line: 0 })
            }
            (_, Some(_)) => {
                return Err(error(
                    keys.span("value"),
                    "`value` is a string-backed case's",
                ));
            }
        };
        cases.push(quote!(#root::__private::NativeClassConst {
            name: #case,
            visibility: #root::decl::Visibility::Public,
            is_final: false,
            is_case: true,
            ty: || None,
            value: || #value,
            attributes: &[],
        }));
    }
    Ok((table, cases))
}

/// A Rust `enum` as a PHP enum: its variants are the cases in order, a
/// string-backed case's value its `#[php(value = "..")]`, an int-backed
/// one's its discriminant. php lists `BackedEnum` before `UnitEnum`. A
/// body takes and returns the Rust variant.
fn enumeration(attribute: Tokens, mut item: ItemEnum) -> syn::Result<Tokens> {
    let root = root();
    let keys = Keys::parse(attribute)?;
    keys.allow(ENUM_KEYS, "an enum header")?;
    let ident = item.ident.clone();
    let php = expr::qualified(&keys, &ident)?;
    let class_name = bytes(&php);
    let backing = match keys.eq("backed").map(ToString::to_string).as_deref() {
        None => Backing::Pure,
        Some("string") => Backing::String,
        Some("int") => Backing::Int,
        Some(_) => return Err(error(keys.span("backed"), "`backed` is `string` or `int`")),
    };
    let (table, cases) = cases(&mut item, backing, &php)?;
    let mut interfaces = Vec::new();
    let backing = match backing {
        Backing::Pure => quote!(Pure),
        Backing::String | Backing::Int => {
            let ty = if backing == Backing::String {
                quote!(String)
            } else {
                quote!(Int)
            };
            interfaces.push(bytes("BackedEnum").into_token_stream());
            ty
        }
    };
    interfaces.push(bytes("UnitEnum").into_token_stream());
    interfaces.extend(class_names(&paths(keys.list("implements"))?));
    let attributes = expr::built(&expr::attributes(&keys)?);
    let class = class_impl(&ident, &php, &quote!(()), &quote!(), (false, false));
    // An enum is final and takes no dynamic property, as the compiler
    // declares a program's.
    let header = header_impl(
        &ident,
        &Entry {
            name: class_name.clone(),
            kind: quote!(Enum),
            flags: class_flags(&[("is_final", true)], &quote!(Forbidden)),
            parent: quote!(None),
            interfaces,
            implementors: Vec::new(),
            aliases: Vec::new(),
            backing,
            constants: cases,
            attributes,
            object_size: quote!(0),
            object_beside: Tokens::new(),
            objects: quote!(None),
        },
    );
    let declared = quote!(Some(#root::decl::Type::named(#root::decl::Name::borrowed(#class_name))));
    Ok(quote! {
        #[derive(Clone, Copy, PartialEq)]
        #item
        #class
        impl #root::abi::Cases for #ident {
            const CASES: &'static [(&'static [u8], Self)] = &[#(#table),*];
        }
        impl #root::abi::FromArgument for #ident {
            const MASK: u32 = 0;
            const DECLARED: Option<#root::decl::Type> = #declared;
            fn accepts(value: &#root::values::Value) -> bool {
                matches!(value.as_ref(), #root::values::ValueRef::Object(_))
            }
            fn from_value(
                request: &mut #root::request::Request,
                value: #root::values::Value,
            ) -> Result<Self, #root::raising::Thrown> {
                #root::__private::case_of::<Self>(request, &value)
            }
        }
        impl #root::abi::IntoReturn for #ident {
            const MASK: u32 = 0;
            const DECLARED: Option<#root::decl::Type> = #declared;
            fn into_return(
                self,
                request: &mut #root::request::Request,
            ) -> Result<#root::values::Owner<#root::values::Value>, #root::raising::Thrown> {
                #root::__private::case_value(request, self)
            }
        }
        #header
    })
}

fn visibility(keys: &Keys) -> Tokens {
    let root = root();
    if keys.has("private") {
        quote!(#root::decl::Visibility::Private)
    } else if keys.has("protected") {
        quote!(#root::decl::Visibility::Protected)
    } else {
        quote!(#root::decl::Visibility::Public)
    }
}

const METHOD_KEYS: &[(&str, Form)] = &[
    ("protected", Form::Flag),
    ("private", Form::Flag),
    ("final", Form::Flag),
    ("tentative", Form::Flag),
    ("implementation", Form::Eq),
    ("arity", Form::Eq),
    ("before_args", Form::Eq),
    ("deprecated", Form::FlagOrList),
    ("no_discard", Form::FlagOrList),
    ("attribute", Form::List),
];

const PROPERTY_KEYS: &[(&str, Form)] = &[
    ("property", Form::Flag),
    ("protected", Form::Flag),
    ("private", Form::Flag),
    ("readonly", Form::Flag),
    ("static", Form::Flag),
    ("virtual", Form::Flag),
    ("attribute", Form::List),
];

const CONSTANT_KEYS: &[(&str, Form)] = &[
    ("protected", Form::Flag),
    ("private", Form::Flag),
    ("final", Form::Flag),
    ("deprecated", Form::FlagOrList),
    ("attribute", Form::List),
];

/// One method of a member impl.
struct Method {
    sig: syn::Signature,
    keys: Keys,
    signature: Signature,
    body: bool,
    cfg: Vec<Attribute>,
}

/// One property: its name, type, keys, default and `#[cfg]`s.
struct Property {
    ident: Ident,
    ty: Type,
    keys: Keys,
    default: Option<syn::Expr>,
    cfg: Vec<Attribute>,
}

/// A body-less `fn` or a `const` with no value, which rustc hands the
/// attribute as tokens.
enum Bodiless {
    Method(Vec<Attribute>, syn::Signature),
    Property(Vec<Attribute>, Ident, Type),
}

fn bodiless(input: ParseStream<'_>) -> syn::Result<Bodiless> {
    let attrs = input.call(Attribute::parse_outer)?;
    input.parse::<syn::Visibility>()?;
    if input.peek(syn::Token![const]) {
        input.parse::<syn::Token![const]>()?;
        let ident = Ident::parse_any(input)?;
        input.parse::<syn::Token![:]>()?;
        let ty: Type = input.parse()?;
        input.parse::<syn::Token![;]>()?;
        return Ok(Bodiless::Property(attrs, ident, ty));
    }
    let sig: syn::Signature = input.parse()?;
    input.parse::<syn::Token![;]>()?;
    Ok(Bodiless::Method(attrs, sig))
}

/// A class constant: its name, type, keys and `#[cfg]`s.
type Constant = (Ident, Type, Keys, Vec<Attribute>);

/// A member impl's items, sorted by kind in source order.
struct Collected {
    methods: Vec<Method>,
    constants: Vec<Constant>,
    properties: Vec<Property>,
}

/// Sort a member impl's items, leaving in it what Rust keeps: the methods
/// with bodies and the class constants.
fn collect(block: &mut ItemImpl) -> syn::Result<Collected> {
    let mut out = Collected {
        methods: Vec::new(),
        constants: Vec::new(),
        properties: Vec::new(),
    };
    let mut kept = Vec::new();
    for item in std::mem::take(&mut block.items) {
        match item {
            ImplItem::Fn(mut function) => {
                let keys = Keys::take(&mut function.attrs)?;
                let signature = function::read(&mut function.sig)?;
                let cfg = cfgs(&function.attrs);
                out.methods.push(Method {
                    sig: function.sig.clone(),
                    keys,
                    signature,
                    body: true,
                    cfg,
                });
                kept.push(ImplItem::Fn(function));
            }
            ImplItem::Verbatim(tokens) => match bodiless.parse2(tokens)? {
                Bodiless::Method(mut attrs, mut sig) => {
                    let keys = Keys::take(&mut attrs)?;
                    let signature = function::read(&mut sig)?;
                    let cfg = cfgs(&attrs);
                    out.methods.push(Method {
                        sig,
                        keys,
                        signature,
                        body: false,
                        cfg,
                    });
                }
                Bodiless::Property(mut attrs, ident, ty) => {
                    let keys = Keys::take(&mut attrs)?;
                    if !keys.has("property") {
                        return Err(error(
                            ident.span(),
                            "a class constant has a value; a property without one is `#[php(property)]`",
                        ));
                    }
                    let cfg = cfgs(&attrs);
                    out.properties.push(Property {
                        ident,
                        ty,
                        keys,
                        default: None,
                        cfg,
                    });
                }
            },
            ImplItem::Const(mut constant) => {
                let keys = Keys::take(&mut constant.attrs)?;
                let cfg = cfgs(&constant.attrs);
                if keys.has("property") {
                    // Removed: the value is re-emitted in `PROPERTIES`,
                    // checked against the property type's `Const`.
                    out.properties.push(Property {
                        ident: constant.ident.clone(),
                        ty: constant.ty.clone(),
                        keys,
                        default: Some(constant.expr.clone()),
                        cfg,
                    });
                } else {
                    keys.allow(CONSTANT_KEYS, "a class constant")?;
                    out.constants
                        .push((constant.ident.clone(), constant.ty.clone(), keys, cfg));
                    kept.push(ImplItem::Const(constant));
                }
            }
            other => {
                return Err(error(
                    other.span(),
                    "a member impl holds methods, constants and properties",
                ));
            }
        }
    }
    block.items = kept;
    Ok(out)
}

/// A class constant's static entry, typed and valued by `IntoConst`.
fn constant_decl(class: &Ident, (constant, ty, keys, cfg): &Constant) -> syn::Result<Tokens> {
    let root = root();
    let php = bytes(&php_name(constant));
    let visibility = visibility(keys);
    let is_final = keys.has("final");
    let attributes = expr::built(&expr::attributes(keys)?);
    Ok(quote!(#(#cfg)* #root::__private::NativeClassConst {
        name: #php,
        visibility: #visibility,
        is_final: #is_final,
        is_case: false,
        ty: || <#ty as #root::abi::IntoConst>::DECLARED,
        value: || #root::abi::IntoConst::value(&#class::#constant),
        attributes: #attributes,
    }))
}

fn members(attribute: Tokens, mut block: ItemImpl) -> syn::Result<Tokens> {
    let root = root();
    let keys = Keys::parse(attribute)?;
    keys.allow(&[("trampolines", Form::Flag)], "a member impl")?;
    if block.trait_.is_some() || !block.generics.params.is_empty() {
        return Err(error(
            block.span(),
            "`#[php_class]` is on `impl Class`, no trait, no generics",
        ));
    }
    let Type::Path(self_ty) = block.self_ty.as_ref() else {
        return Err(error(
            block.self_ty.span(),
            "`#[php_class]` is on `impl Class`",
        ));
    };
    let ident = self_ty
        .path
        .segments
        .last()
        .map(|segment| segment.ident.clone())
        .ok_or_else(|| error(self_ty.span(), "a class"))?;

    let Collected {
        methods,
        constants,
        properties,
    } = collect(&mut block)?;

    let trampolines = keys.has("trampolines");
    let mut declarations = Vec::new();
    let mut entries = Vec::new();
    let mut checks = Vec::new();
    for method in &methods {
        let (decl, entry, method_checks) = method_pieces(&ident, method)?;
        declarations.push(decl);
        entries.extend(entry);
        checks.extend(method_checks);
    }
    if trampolines {
        checks.push(quote!(const _: () = assert!(#ident::__PHP_EXTERN, "`trampolines` is a key of an impl on a class declared elsewhere");));
    }
    if !methods.is_empty() && !trampolines {
        checks.push(quote!(const _: () = assert!(!#ident::__PHP_EXTERN, "a method on a class declared elsewhere is a trampoline: `#[php_class(trampolines)]`");));
    }
    let constant_decls = constants
        .iter()
        .map(|constant| constant_decl(&ident, constant))
        .collect::<syn::Result<Vec<_>>>()?;
    let property_decls = properties
        .iter()
        .map(|property| property_decl(&ident, property, &mut checks))
        .collect::<syn::Result<Vec<_>>>()?;
    Ok(quote! {
        #[allow(non_snake_case, non_upper_case_globals, clippy::too_many_arguments, clippy::needless_pass_by_value)]
        #block
        #[allow(non_snake_case)]
        impl #ident {
            #(#entries)*
        }
        #(#checks)*
        impl #root::abi::Members for #ident {
            const CONSTANTS: &'static [#root::__private::NativeClassConst] = &[#(#constant_decls),*];
            const PROPERTIES: &'static [#root::__private::NativeProperty] = &[#(#property_decls),*];
            const METHODS: &'static [#root::__private::NativeMethod] = &[#(#declarations),*];
        }
    })
}

fn property_decl(
    class: &Ident,
    property: &Property,
    checks: &mut Vec<Tokens>,
) -> syn::Result<Tokens> {
    let root = root();
    property.keys.allow(PROPERTY_KEYS, "a property")?;
    let php = bytes(&php_name(&property.ident));
    let visibility = visibility(&property.keys);
    let readonly = property.keys.has("readonly");
    let instance = !property.keys.has("static");
    let is_virtual = property.keys.has("virtual");
    if is_virtual && property.default.is_some() {
        return Err(error(
            property.keys.span("virtual"),
            "a virtual property has no slot, so no default (`@virtual`)",
        ));
    }
    let ty = &property.ty;
    let default = property.default.as_ref().map_or_else(
        || quote!(None),
        |value| quote!(Some(|| <#ty as #root::abi::PropertyValue>::const_expr(::core::convert::Into::into(#value)))),
    );
    let attributes = expr::built(&expr::attributes(&property.keys)?);
    let cfg = &property.cfg;
    let checked = unself(ty, Some(class));
    checks.push(quote!(#(#cfg)* const _: u32 = <#checked as #root::abi::FromArgument>::MASK;));
    checks.push(quote!(#(#cfg)* const _: () = assert!(!#class::__PHP_EXTERN, "a class declared elsewhere takes no property here");));
    Ok(quote!(#(#cfg)* #root::__private::NativeProperty {
        name: #php,
        visibility: #visibility,
        is_instance: #instance,
        is_readonly: #readonly,
        is_virtual: #is_virtual,
        ty: || <#ty as #root::abi::FromArgument>::DECLARED,
        default: #default,
        attributes: #attributes,
    }))
}

/// The entry of a method with a body: the prologue, the receiver, the
/// body, the return.
fn method_entry(class: &Ident, method: &Method, entry: &Ident, reads: &function::Reads) -> Tokens {
    let root = root();
    let ident = &method.sig.ident;
    let cfg = &method.cfg;
    let prologue = function::prologue(
        &method.signature.params,
        &quote!(Some(<#class as #root::abi::Class>::NAME)),
        reads,
        (Some(class), ident),
    );
    let mut arguments = Vec::new();
    if method.signature.request {
        arguments.push(quote!(__request));
    }
    let this = method.signature.this.as_ref().map(|ty| {
        arguments.push(quote!(this));
        quote!(let this: #ty = <#ty>::read(__request)?;)
    });
    arguments.extend(method.signature.params.iter().map(|param| {
        let ident = &param.ident;
        quote!(#ident)
    }));
    quote! {
        #(#cfg)*
        #[doc(hidden)]
        pub(crate) fn #entry(
            __request: &mut #root::request::Request,
        ) -> Result<#root::values::Owner<#root::values::Value>, #root::raising::Thrown> {
            #prologue
            #this
            #root::abi::IntoReturn::into_return(Self::#ident(#(#arguments),*), __request)
        }
    }
}

/// A method's declaration, its entry where it has a body, and its checks.
fn method_pieces(
    class: &Ident,
    method: &Method,
) -> syn::Result<(Tokens, Option<Tokens>, Vec<Tokens>)> {
    let root = root();
    method.keys.allow(METHOD_KEYS, "a method")?;
    let ident = &method.sig.ident;
    let php = php_name(ident);
    let name = bytes(&php);
    let is_final = method.keys.has("final");
    let tentative = method.keys.has("tentative");
    let structor =
        php.eq_ignore_ascii_case("__construct") || php.eq_ignore_ascii_case("__destruct");
    let implementation = method.keys.eq("implementation");
    let reads = function::Reads::of(&method.keys)?;
    if method.body && implementation.is_some() {
        return Err(error(
            method.keys.span("implementation"),
            "`implementation =` names another method's entry; this method has a body of its own",
        ));
    }
    if tentative && (is_final || structor) {
        return Err(error(
            method.keys.span("tentative"),
            "a tentative return is refused on a final method and on __construct and __destruct (design §4.7)",
        ));
    }
    let is_abstract = !method.body && implementation.is_none();
    let cfg = &method.cfg;
    let mut checks: Vec<Tokens> = function::checks(&method.signature, Some(class))
        .into_iter()
        .map(|check| quote!(#(#cfg)* #check))
        .collect();
    let output = unself(&method.signature.output, Some(class));
    if tentative {
        checks.push(quote!(#(#cfg)* const _: () = assert!(!<#output as #root::abi::IntoReturn>::UNDECLARED, "a tentative return declares a type (design §4.7)");));
    }
    if is_abstract {
        let text = format!(
            "`{ident}` has no body, which needs an interface, an abstract class or `implementation =`"
        );
        checks.push(quote!(#(#cfg)* const _: () = assert!(#class::__PHP_ABSTRACT, #text);));
    }
    let entry_ident = format_ident!("__php_entry_{}", ident.unraw());
    let entry_path = if let Some(path) = implementation {
        let mut path: syn::Path = syn::parse2(path.clone())?;
        if let Some(last) = path.segments.last_mut() {
            last.ident = format_ident!("__php_entry_{}", last.ident.unraw());
        }
        quote!(#path as #root::__private::Entry)
    } else if is_abstract {
        quote!(#root::__private::abstract_entry as #root::__private::Entry)
    } else {
        quote!(#class::#entry_ident as #root::__private::Entry)
    };
    let parameters = function::declared_parameters(&method.signature.params)?;
    let signature_output = &method.signature.output;
    let return_type = if structor {
        quote!(|| None)
    } else {
        quote!(|| <#signature_output as #root::abi::IntoReturn>::DECLARED)
    };
    let visibility = visibility(&method.keys);
    let is_instance = method.signature.this.is_some();
    let attributes = expr::built(&expr::attributes(&method.keys)?);
    let declaration = quote!(#(#cfg)* #root::__private::NativeMethod {
        function: #root::__private::NativeFunction {
            name: #name,
            parameters: #parameters,
            return_type: #return_type,
            frameless: &[],
            attributes: #attributes,
            entry: #entry_path,
        },
        visibility: #visibility,
        is_instance: #is_instance,
        is_abstract: #is_abstract,
        is_final: #is_final,
        tentative_return: #tentative,
    });
    let entry = method
        .body
        .then(|| method_entry(class, method, &entry_ident, &reads));
    Ok((declaration, entry, checks))
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::expand;
    use crate::refused;

    #[test]
    fn a_class_header_the_attribute_cannot_declare_is_refused() {
        for (attribute, item, message) in [
            (
                quote!(),
                quote!(
                    fn f() {}
                ),
                "php_macros: `#[php_class]` is on a struct, an enum or an impl",
            ),
            (
                quote!(new = make),
                quote!(
                    struct C;
                ),
                "php_macros: `new` lays a payload down; the header names none",
            ),
            (
                quote!(dynamic_properties = sometimes),
                quote!(
                    struct C;
                ),
                "php_macros: `dynamic_properties` is `forbidden`, `deprecated` or `allowed`",
            ),
            (
                quote!(),
                quote!(
                    struct C<T>(T);
                ),
                "php_macros: a class header has no generics",
            ),
            (
                quote!(extends(A, B)),
                quote!(
                    struct C;
                ),
                "php_macros: a class extends one parent",
            ),
            (
                quote!(beside(8)),
                quote!(
                    struct C;
                ),
                "php_macros: `beside(..)` goes with the `size = N` of the struct it is allocated beside",
            ),
        ] {
            assert_eq!(refused(expand(attribute, item.clone())), message, "{item}");
        }
    }

    #[test]
    fn an_enum_the_attribute_cannot_declare_is_refused() {
        for (attribute, item, message) in [
            (
                quote!(),
                quote!(
                    enum E {
                        A(i64),
                    }
                ),
                "php_macros: an enum case carries no fields",
            ),
            (
                quote!(backed = string),
                quote!(
                    enum E {
                        A,
                    }
                ),
                "php_macros: a string-backed case takes `#[php(value = \"..\")]`",
            ),
            (
                quote!(),
                quote!(
                    enum E {
                        #[php(value = "a")]
                        A,
                    }
                ),
                "php_macros: `value` is a string-backed case's",
            ),
            (
                quote!(backed = int),
                quote!(
                    enum E {
                        #[php(value = "a")]
                        A,
                    }
                ),
                "php_macros: `value` is a string-backed case's",
            ),
            (
                quote!(backed = float),
                quote!(
                    enum E {
                        A,
                    }
                ),
                "php_macros: `backed` is `string` or `int`",
            ),
        ] {
            assert_eq!(refused(expand(attribute, item.clone())), message, "{item}");
        }
    }

    #[test]
    fn a_member_impl_the_attribute_cannot_declare_is_refused() {
        for (item, message) in [
            (
                quote!(
                    impl C {
                        const X: i64;
                    }
                ),
                "php_macros: a class constant has a value; a property without one is `#[php(property)]`",
            ),
            (
                quote!(
                    impl C {
                        type T = i64;
                    }
                ),
                "php_macros: a member impl holds methods, constants and properties",
            ),
            (
                quote!(
                    impl Display for C {}
                ),
                "php_macros: `#[php_class]` is on `impl Class`, no trait, no generics",
            ),
            (
                quote!(
                    impl<T> C<T> {}
                ),
                "php_macros: `#[php_class]` is on `impl Class`, no trait, no generics",
            ),
            (
                quote!(
                    impl [u8] {}
                ),
                "php_macros: `#[php_class]` is on `impl Class`",
            ),
            (
                quote!(
                    impl C {
                        #[php(property, virtual)]
                        const x: i64 = 1;
                    }
                ),
                "php_macros: a virtual property has no slot, so no default (`@virtual`)",
            ),
            (
                quote!(
                    impl C {
                        #[php(implementation = D::f)]
                        fn f() {}
                    }
                ),
                "php_macros: `implementation =` names another method's entry; this method has a body of its own",
            ),
            (
                quote!(
                    impl C {
                        #[php(tentative, final)]
                        fn f() -> i64 {
                            0
                        }
                    }
                ),
                "php_macros: a tentative return is refused on a final method and on __construct and __destruct (design §4.7)",
            ),
            (
                quote!(
                    impl C {
                        #[php(tentative)]
                        fn __construct() {}
                    }
                ),
                "php_macros: a tentative return is refused on a final method and on __construct and __destruct (design §4.7)",
            ),
        ] {
            assert_eq!(refused(expand(quote!(), item.clone())), message, "{item}");
        }
    }
}
