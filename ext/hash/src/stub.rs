//! One native's declaration read off the line `hash.stub.php` writes
//! for it, so the declaration model is php's own text: parameter types
//! with `?` and `|`, `#[\SensitiveParameter]`, an untyped `$stream`, and
//! a default that is `false`, `null`, `[]`, a number or a string.

use php_ext::decl::{Attribute, ConstExpr, FunctionDecl, Name, Parameter, Type};

/// The declaration a stub line writes.
///
/// ```php
/// echo new ReflectionFunction('hash_hmac'), new ReflectionFunction('hash_update_stream');
/// ```
/// ```expect
/// Function [ <internal:hash> function hash_hmac ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> string $algo ]
///     Parameter #1 [ <required> string $data ]
///     Parameter #2 [ <required> string $key ]
///     Parameter #3 [ <optional> bool $binary = false ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:hash> function hash_update_stream ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> HashContext $context ]
///     Parameter #1 [ <required> $stream ]
///     Parameter #2 [ <optional> int $length = -1 ]
///   }
///   - Return [ int ]
/// }
/// ```
pub(crate) fn stub(line: &str) -> FunctionDecl {
    let (name, rest) = line.split_once('(').unwrap_or((line, ")"));
    let (params, returns) = rest.rsplit_once(')').unwrap_or((rest, ""));
    let parameters = params
        .split(", ")
        .filter(|p| !p.is_empty())
        .map(parameter)
        .collect();
    let returns = returns.trim_start_matches(':').trim();
    FunctionDecl {
        name: Name(name.as_bytes().into()),
        parameters,
        return_type: (!returns.is_empty()).then(|| ty(returns)),
        returns_by_ref: false,
        is_generator: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
        frameless: Vec::new(),
    }
}

/// `[#[\SensitiveParameter]] [T] $name [= default]`.
fn parameter(text: &str) -> Parameter {
    let (attributes, text) = match text.strip_prefix("#[\\SensitiveParameter] ") {
        Some(rest) => (
            vec![Attribute {
                name: Name(b"SensitiveParameter".as_slice().into()),
                arguments: Vec::new(),
            }],
            rest,
        ),
        None => (Vec::new(), text),
    };
    let (declared, rest) = text.split_once('$').unwrap_or(("", text));
    let (name, default) = match rest.split_once(" = ") {
        Some((name, default)) => (name, Some(constant(default))),
        None => (rest, None),
    };
    let declared = declared.trim();
    Parameter {
        name: Name(name.as_bytes().into()),
        ty: (!declared.is_empty()).then(|| ty(declared)),
        by_ref: false,
        prefer_ref: false,
        variadic: false,
        default,
        attributes,
    }
}

/// `?T`, or `A|B`.
fn ty(text: &str) -> Type {
    if let Some(inner) = text.strip_prefix('?') {
        return Type::Nullable(Box::new(ty(inner)));
    }
    let mut members: Vec<Type> = text
        .split('|')
        .map(|word| match word {
            "mixed" => Type::Mixed,
            "void" => Type::Void,
            "null" => Type::Null,
            "bool" => Type::Bool,
            "false" => Type::False,
            "true" => Type::True,
            "int" => Type::Int,
            "string" => Type::String,
            "array" => Type::Array,
            _ => Type::Named(Name(word.as_bytes().into())),
        })
        .collect();
    if members.len() == 1 {
        members.pop().unwrap_or(Type::Mixed)
    } else {
        Type::Union(members)
    }
}

fn constant(text: &str) -> ConstExpr {
    match text {
        "false" => ConstExpr::Bool(false),
        "true" => ConstExpr::Bool(true),
        "null" => ConstExpr::Null,
        "[]" => ConstExpr::Array(Vec::new()),
        _ => match text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
            Some(string) => ConstExpr::Str(string.as_bytes().into()),
            None => ConstExpr::Long(text.parse().unwrap_or(0)),
        },
    }
}
