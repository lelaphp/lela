//! php's constant expression written as Rust tokens, into the tokens that
//! build its `ConstExpr` (design §7.1, R5): literals, constants, class
//! constants and enum cases, the operators, arrays and `new`. An integer
//! is read as php's lexer reads it, so `0777` is octal 511 and not Rust's
//! decimal 777. Nothing here evaluates: the engine does, in the declaring
//! class's scope, when an argument is absent. Every node's line is zero:
//! no file holds a native's expression, so none names one.
//!
//! Also the php attributes the keys spell: `deprecated(..)`,
//! `no_discard(..)`, `sensitive` and the generic `attribute(Path(..))`.

use proc_macro2::TokenStream as Tokens;
use quote::quote;
use syn::ext::IdentExt;
use syn::parse::{ParseStream, Parser};
use syn::{Ident, Token};

use crate::keys::Keys;
use crate::{bytes, error, name, php_name, root};

/// The `ConstExpr` of `tokens`.
pub(crate) fn const_expr(tokens: Tokens) -> syn::Result<Tokens> {
    let parser = |input: ParseStream<'_>| {
        let expr = binary(input, 0)?;
        if !input.is_empty() {
            return Err(input.error("php_macros: a constant expression ends here"));
        }
        Ok(expr)
    };
    parser.parse2(tokens)
}

/// The `Absent` an entry lays down for a parameter whose default is
/// `tokens`: a scalar written as itself as its value and a string
/// written as itself as its bytes, which the call takes with nothing
/// evaluated, as php's C default is a constant; anything else as the
/// function that builds its `ConstExpr`, which the request calls once,
/// where it evaluates the expression first.
pub(crate) fn absent(tokens: Tokens) -> syn::Result<Tokens> {
    let root = root();
    let literal = |input: ParseStream<'_>| -> syn::Result<Option<Tokens>> {
        let negative = input.peek(Token![-]);
        if negative {
            input.parse::<Token![-]>()?;
        }
        let absent = if input.peek(syn::LitInt) {
            let (value, _) = php_int(&input.parse()?)?;
            let value = if negative {
                value.wrapping_neg()
            } else {
                value
            };
            quote!(#root::abi::Absent::Value(#root::values::Value::int(#value)))
        } else if input.peek(syn::LitFloat) {
            let value: f64 = input.parse::<syn::LitFloat>()?.base10_parse()?;
            let value = if negative { -value } else { value };
            quote!(#root::abi::Absent::Value(#root::values::Value::float(#value)))
        } else if negative {
            return Ok(None);
        } else if input.peek(syn::LitStr) {
            let value = bytes(&input.parse::<syn::LitStr>()?.value());
            quote!(#root::abi::Absent::Str(#value))
        } else if input.peek(syn::LitByteStr) {
            let literal: syn::LitByteStr = input.parse()?;
            quote!(#root::abi::Absent::Str(#literal))
        } else if input.peek(syn::LitBool) {
            let value = input.parse::<syn::LitBool>()?.value;
            quote!(#root::abi::Absent::Value(#root::values::Value::bool(#value)))
        } else if input
            .fork()
            .call(Ident::parse_any)
            .is_ok_and(|word| word == "null")
        {
            input.call(Ident::parse_any)?;
            quote!(#root::abi::Absent::Value(#root::values::Value::null()))
        } else {
            return Ok(None);
        };
        Ok(input.is_empty().then_some(absent))
    };
    if let Ok(Some(absent)) = literal.parse2(tokens.clone()) {
        return Ok(absent);
    }
    let expr = const_expr(tokens)?;
    Ok(quote!(#root::abi::Absent::Expr(|| #expr)))
}

/// php's binary operators over constant expressions, by precedence
/// (`zend_language_parser.y`), the loosest first.
fn operator(input: ParseStream<'_>) -> Option<(&'static str, u8)> {
    Some(if input.peek(Token![?]) && input.peek2(Token![?]) {
        ("??", 0)
    } else if input.peek(Token![||]) {
        ("||", 1)
    } else if input.peek(Token![&&]) {
        ("&&", 2)
    } else if input.peek(Token![|]) {
        ("|", 3)
    } else if input.peek(Token![^]) {
        ("^", 4)
    } else if input.peek(Token![&]) {
        ("&", 5)
    } else if input.peek(Token![<<]) {
        ("<<", 6)
    } else if input.peek(Token![>>]) {
        (">>", 6)
    } else if input.peek(Token![.]) {
        (".", 6)
    } else if input.peek(Token![+]) {
        ("+", 7)
    } else if input.peek(Token![-]) {
        ("-", 7)
    } else if input.peek(Token![*]) {
        ("*", 8)
    } else if input.peek(Token![/]) {
        ("/", 8)
    } else if input.peek(Token![%]) {
        ("%", 8)
    } else {
        return None;
    })
}

fn binary(input: ParseStream<'_>, min: u8) -> syn::Result<Tokens> {
    let root = root();
    let mut lhs = unary(input)?;
    while let Some((op, precedence)) = operator(input) {
        if precedence < min {
            break;
        }
        for _ in 0..op.len() {
            input.parse::<proc_macro2::Punct>()?;
        }
        let rhs = binary(input, precedence + 1)?;
        let op = name(op);
        lhs = quote!(#root::decl::ConstExpr::Binary { op: #op, lhs: Box::new(#lhs), rhs: Box::new(#rhs), line: 0 });
    }
    Ok(lhs)
}

/// An integer token as php's lexer reads it, with the radix the model
/// keeps for Reflection to render: `0o644` explicit octal, `0777` octal,
/// anything else decimal. A stub writes hex and binary only in a
/// constant's value, which is declared by value, so the model has no
/// radix for them and a default in either is refused here.
fn php_int(literal: &syn::LitInt) -> syn::Result<(i64, Tokens)> {
    let text: String = literal.to_string().chars().filter(|c| *c != '_').collect();
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("0x") || lower.starts_with("0b") {
        return Err(error(
            literal.span(),
            "a default is written in decimal or octal, the radixes the declaration model keeps",
        ));
    }
    let (digits, base, radix) = if let Some(digits) = lower.strip_prefix("0o") {
        (digits, 8, quote!(OctalExplicit))
    } else if lower.len() > 1 && lower.starts_with('0') {
        (&lower[1..], 8, quote!(Octal))
    } else {
        (lower.as_str(), 10, quote!(Decimal))
    };
    if !literal.suffix().is_empty() {
        return Err(error(literal.span(), "a php integer carries no suffix"));
    }
    i64::from_str_radix(digits, base)
        .map(|value| (value, radix))
        .map_err(|_| error(literal.span(), &format!("`{text}` is not a php integer")))
}

/// `ConstExpr::Long` of a value in its radix.
fn long(value: i64, radix: &Tokens) -> Tokens {
    let root = root();
    quote!(#root::decl::ConstExpr::Long { value: #value, radix: #root::decl::Radix::#radix })
}

fn unary(input: ParseStream<'_>) -> syn::Result<Tokens> {
    let root = root();
    let op = if input.peek(Token![-]) {
        "-"
    } else if input.peek(Token![+]) {
        "+"
    } else if input.peek(Token![!]) {
        "!"
    } else if input.peek(Token![~]) {
        "~"
    } else {
        return primary(input);
    };
    input.parse::<proc_macro2::Punct>()?;
    if op == "-" && input.peek(syn::LitInt) {
        let literal: syn::LitInt = input.parse()?;
        let (value, radix) = php_int(&literal)?;
        return Ok(long(value.wrapping_neg(), &radix));
    }
    if op == "-" && input.peek(syn::LitFloat) {
        let literal: syn::LitFloat = input.parse()?;
        let value = -literal.base10_parse::<f64>()?;
        return Ok(quote!(#root::decl::ConstExpr::Double(#value)));
    }
    let operand = unary(input)?;
    let op = name(op);
    Ok(quote!(#root::decl::ConstExpr::Unary { op: #op, operand: Box::new(#operand), line: 0 }))
}

fn primary(input: ParseStream<'_>) -> syn::Result<Tokens> {
    let root = root();
    if input.peek(syn::LitInt) {
        let (value, radix) = php_int(&input.parse()?)?;
        return Ok(long(value, &radix));
    }
    if input.peek(syn::LitFloat) {
        let literal: syn::LitFloat = input.parse()?;
        let value: f64 = literal.base10_parse()?;
        return Ok(quote!(#root::decl::ConstExpr::Double(#value)));
    }
    if input.peek(syn::LitStr) {
        let literal: syn::LitStr = input.parse()?;
        let value = bytes(&literal.value());
        return Ok(quote!(#root::decl::ConstExpr::Str(#value.as_slice().into())));
    }
    if input.peek(syn::LitByteStr) {
        let literal: syn::LitByteStr = input.parse()?;
        return Ok(quote!(#root::decl::ConstExpr::Str(#literal.as_slice().into())));
    }
    if input.peek(syn::LitBool) {
        let value = input.parse::<syn::LitBool>()?.value;
        return Ok(quote!(#root::decl::ConstExpr::Bool(#value)));
    }
    if input.peek(syn::token::Paren) {
        let inner;
        syn::parenthesized!(inner in input);
        return binary(&inner, 0);
    }
    if input.peek(syn::token::Bracket) {
        let inner;
        syn::bracketed!(inner in input);
        return array(&inner);
    }
    if input.peek(Token![const]) {
        // `const(Ns::NAME)`: a namespaced global constant, which `::`
        // alone would read as a class constant.
        input.parse::<Token![const]>()?;
        let inner;
        syn::parenthesized!(inner in input);
        let constant = name(&path(&inner)?.join("\\"));
        return Ok(
            quote!(#root::decl::ConstExpr::Constant { name: #root::decl::ConstantName::Resolved(#constant), line: 0 }),
        );
    }
    let first = Ident::parse_any(input)?;
    if first == "null" {
        return Ok(quote!(#root::decl::ConstExpr::Null));
    }
    if first == "new" {
        let class = name(&path(input)?.join("\\"));
        let inner;
        syn::parenthesized!(inner in input);
        let mut arguments = Vec::new();
        while !inner.is_empty() {
            let value = binary(&inner, 0)?;
            arguments.push(quote!((None, #value)));
            if !inner.is_empty() {
                inner.parse::<Token![,]>()?;
            }
        }
        return Ok(
            quote!(#root::decl::ConstExpr::New { class: #class, arguments: vec![#(#arguments),*], line: 0 }),
        );
    }
    let mut segments = vec![php_name(&first)];
    while input.peek(Token![::]) {
        input.parse::<Token![::]>()?;
        segments.push(php_name(&Ident::parse_any(input)?));
    }
    let Some((member, class)) = segments.split_last().filter(|(_, class)| !class.is_empty()) else {
        let constant = name(&segments.join("\\"));
        return Ok(
            quote!(#root::decl::ConstExpr::Constant { name: #root::decl::ConstantName::Resolved(#constant), line: 0 }),
        );
    };
    // The last `::` is member access and the segments before it are the
    // class, `\`-joined. An enum case is a class constant here: the engine
    // fetches the case object for it (`zend_enum_get_case`).
    let class = name(&class.join("\\"));
    // `C::class`, which names the class and no constant of it.
    if member.eq_ignore_ascii_case("class") {
        return Ok(quote!(#root::decl::ConstExpr::ClassName { class: #class, line: 0 }));
    }
    let member = name(member);
    Ok(quote!(#root::decl::ConstExpr::ClassConstant {
        class: #class,
        class_kind: #root::decl::NameKind::NotFullyQualified,
        name: #member,
        line: 0,
    }))
}

fn array(input: ParseStream<'_>) -> syn::Result<Tokens> {
    let root = root();
    let mut elements = Vec::new();
    while !input.is_empty() {
        let first = binary(input, 0)?;
        elements.push(if input.peek(Token![=>]) {
            input.parse::<Token![=>]>()?;
            let value = binary(input, 0)?;
            quote!((Some(#first), #value))
        } else {
            quote!((None, #first))
        });
        if !input.is_empty() {
            input.parse::<Token![,]>()?;
        }
    }
    Ok(quote!(#root::decl::ConstExpr::Array { elements: vec![#(#elements),*], line: 0 }))
}

/// `A::B::C` as its php names.
pub(crate) fn path(input: ParseStream<'_>) -> syn::Result<Vec<String>> {
    let mut segments = vec![php_name(&Ident::parse_any(input)?)];
    while input.peek(Token![::]) {
        input.parse::<Token![::]>()?;
        segments.push(php_name(&Ident::parse_any(input)?));
    }
    Ok(segments)
}

/// A namespace key's segments, `namespace(Random::Engine)`.
pub(crate) fn namespace(keys: &Keys) -> syn::Result<Vec<String>> {
    match keys.list("namespace") {
        Some(tokens) => path.parse2(tokens.clone()),
        None => Ok(Vec::new()),
    }
}

/// `ident` under the namespace the keys name, `\`-joined.
pub(crate) fn qualified(keys: &Keys, ident: &Ident) -> syn::Result<String> {
    let mut segments = namespace(keys)?;
    segments.push(php_name(ident));
    Ok(segments.join("\\"))
}

/// The php attributes the keys spell, in php's order: `#[\Deprecated]`,
/// `#[\NoDiscard]`, `#[\SensitiveParameter]`, then each `attribute(..)`.
pub(crate) fn attributes(keys: &Keys) -> syn::Result<Vec<Tokens>> {
    let mut out = Vec::new();
    for (key, php) in [
        ("deprecated", "Deprecated"),
        ("no_discard", "NoDiscard"),
        ("sensitive", "SensitiveParameter"),
        ("attribute_class", "Attribute"),
    ] {
        if keys.has(key) {
            let arguments = match keys.list(key) {
                Some(tokens) => arguments.parse2(tokens.clone())?,
                None => Vec::new(),
            };
            out.push(attribute(php, &arguments));
        }
    }
    for key in keys.all("attribute") {
        out.push(one_attribute.parse2(key.clone())?);
    }
    Ok(out)
}

fn attribute(php: &str, arguments: &[Tokens]) -> Tokens {
    let root = root();
    let php = bytes(php);
    quote!(#root::__private::attribute(#php, vec![#(#arguments),*]))
}

/// `Path(args)` of a generic `attribute(..)` key.
fn one_attribute(input: ParseStream<'_>) -> syn::Result<Tokens> {
    let class = path(input)?.join("\\");
    let arguments = if input.peek(syn::token::Paren) {
        let inner;
        syn::parenthesized!(inner in input);
        arguments(&inner)?
    } else {
        Vec::new()
    };
    Ok(attribute(&class, &arguments))
}

/// An attribute's arguments, each `name = value` or `value`.
fn arguments(input: ParseStream<'_>) -> syn::Result<Vec<Tokens>> {
    let mut out = Vec::new();
    while !input.is_empty() {
        let named = if (input.peek(Ident::peek_any) && input.peek2(Token![=]))
            && !input.peek2(Token![==])
            && !input.peek2(Token![=>])
        {
            let argument = php_name(&Ident::parse_any(input)?);
            input.parse::<Token![=]>()?;
            let argument = bytes(&argument);
            quote!(Some(#argument.as_slice()))
        } else {
            quote!(None)
        };
        let value = binary(input, 0)?;
        out.push(quote!((#named, #value)));
        if !input.is_empty() {
            input.parse::<Token![,]>()?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::const_expr;
    use crate::refused;

    #[test]
    fn what_php_could_not_read_as_a_constant_expression_is_refused() {
        for (tokens, message) in [
            (quote!(1 2), "php_macros: a constant expression ends here"),
            (
                quote!(0x10),
                "php_macros: a default is written in decimal or octal, the radixes the declaration model keeps",
            ),
            (
                quote!(0b10),
                "php_macros: a default is written in decimal or octal, the radixes the declaration model keeps",
            ),
            (quote!(1i64), "php_macros: a php integer carries no suffix"),
            (quote!(09), "php_macros: `09` is not a php integer"),
            (
                quote!(9_223_372_036_854_775_808),
                "php_macros: `9223372036854775808` is not a php integer",
            ),
        ] {
            assert_eq!(refused(const_expr(tokens.clone())), message, "{tokens}");
        }
    }
}
