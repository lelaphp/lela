//! A declared type written as php writes one, `zend_type_to_string`
//! (`Z/Zend/zend_compile.c`), which is the text every message about a
//! type prints: the compiler's redundancy fatals, the linker's
//! incompatible declarations, and Reflection. php writes a type from the
//! `zend_type` it compiled and not from the source, so the order is
//! php's and not the order written: the class names first in the order
//! they were written, an intersection among them in parentheses, then
//! the scalar types in one fixed order, `mixed` alone where the mask is every
//! bit, and `null` as a `?` in front of a single name or as `|null` after
//! a union.

use crate::decl::{MAY_BE_ANY, Type};

/// `zend_type_to_string` over a declared type, with no scope to resolve
/// `self` and `parent` against, which the compiler has already put the
/// class's name in place of where its scope is known.
#[must_use]
pub fn type_to_string(ty: &Type) -> String {
    type_to_string_called(ty, "static")
}

/// [`type_to_string`] with `static` printed as `called`, the name of the
/// called scope, as `zend_type_to_string_resolved` prints it where a
/// call's scope is known: what an argument's and a return's `TypeError`
/// name the type as.
#[must_use]
pub fn type_to_string_called(ty: &Type, called: &str) -> String {
    if let Type::Intersection(members) = ty {
        let mut names = Vec::new();
        for member in members {
            push_names(member, &mut names);
        }
        return names.join("&");
    }
    let mut parts = Vec::new();
    push_names(ty, &mut parts);
    let mask = ty.mask();
    if mask == MAY_BE_ANY {
        parts.push(String::from("mixed"));
        return parts.join("|");
    }
    for (bit, word) in [
        (1 << 15, called),
        (1 << 12, "callable"),
        (1 << 8, "object"),
        (1 << 7, "array"),
        (1 << 6, "string"),
        (1 << 4, "int"),
        (1 << 5, "float"),
    ] {
        if mask & bit != 0 {
            parts.push(String::from(word));
        }
    }
    match mask & 0b1100 {
        0b1100 => parts.push(String::from("bool")),
        0b0100 => parts.push(String::from("false")),
        0b1000 => parts.push(String::from("true")),
        _ => {}
    }
    for (bit, word) in [(1 << 14, "void"), (1 << 17, "never")] {
        if mask & bit != 0 {
            parts.push(String::from(word));
        }
    }
    if mask & (1 << 1) != 0 {
        if let [single] = parts.as_slice()
            && !single.contains('&')
        {
            return format!("?{single}");
        }
        parts.push(String::from("null"));
    }
    parts.join("|")
}

/// The names a type holds, in the order `zend_compile_typename` put
/// them in its list: a class name as written, `Traversable` for
/// `iterable` (`ZEND_TYPE_INIT_CLASS_MASK`), and an intersection inside
/// a union bracketed.
fn push_names(ty: &Type, names: &mut Vec<String>) {
    match ty {
        Type::Named(name) => names.push(String::from_utf8_lossy(&name.0).into_owned()),
        Type::SelfClass => names.push(String::from("self")),
        Type::ParentClass => names.push(String::from("parent")),
        Type::Iterable => names.push(String::from("Traversable")),
        Type::Nullable(inner) => push_names(inner, names),
        Type::Union(members) => {
            for member in members {
                push_names(member, names);
            }
        }
        Type::Intersection(members) => {
            let mut inner = Vec::new();
            for member in members {
                push_names(member, &mut inner);
            }
            names.push(format!("({})", inner.join("&")));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decl::Name;

    fn named(name: &str) -> Type {
        Type::named(Name::from(name))
    }

    #[test]
    fn the_builtins_are_written_in_php_s_order_after_the_names() {
        let ty = Type::Union(
            vec![
                Type::Int,
                named("T"),
                Type::Iterable,
                Type::Object,
                Type::Null,
            ]
            .into(),
        );
        assert_eq!(type_to_string(&ty), "T|Traversable|object|array|int|null");
    }

    #[test]
    fn null_beside_one_name_is_a_question_mark() {
        assert_eq!(
            type_to_string(&Type::Union(vec![Type::Int, Type::Null].into())),
            "?int"
        );
        assert_eq!(
            type_to_string(&Type::Nullable(Box::new(Type::Iterable).into())),
            "Traversable|array|null"
        );
        assert_eq!(type_to_string(&Type::Null), "null");
        assert_eq!(type_to_string(&Type::Mixed), "mixed");
        assert_eq!(type_to_string(&Type::Bool), "bool");
    }

    #[test]
    fn static_is_the_called_scope_where_one_is_named() {
        let ty = Type::Union(vec![Type::CalledClass, Type::Null].into());
        assert_eq!(type_to_string(&ty), "?static");
        assert_eq!(type_to_string_called(&ty, "B"), "?B");
        let ty = Type::Union(vec![named("A"), Type::CalledClass, Type::Int].into());
        assert_eq!(type_to_string_called(&ty, "B"), "A|B|int");
    }

    #[test]
    fn an_intersection_is_bracketed_only_inside_a_union() {
        let ab = Type::Intersection(vec![named("A"), named("B")].into());
        assert_eq!(type_to_string(&ab), "A&B");
        assert_eq!(
            type_to_string(&Type::Union(vec![ab, Type::Null].into())),
            "(A&B)|null"
        );
    }
}
