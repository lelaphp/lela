//! The methods `zend_enum_register_funcs` gives every enum
//! (`Z/Zend/zend_enum.c`): `cases`, and for a backed enum `from` and
//! `tryFrom`, declared as `arginfo_class_UnitEnum_cases` and
//! `arginfo_class_BackedEnum_from` declare them. One list, read by the
//! compiler for an enum a program declares and by the engine for an
//! enum a native declares, so both carry the same three rows in the
//! same order after the enum's own methods.

use crate::decl::{
    ClassDecl, ClassKind, EnumBacking, FunctionDecl, MethodDecl, Parameter, Type, Visibility,
    native_function, parameter,
};

/// The rows, public and no instance member, each with the empty file
/// and line zero a native's declaration carries: `cases` returning an
/// array, then `from` returning the called class and `tryFrom` it or
/// `null`, each over `int|string $value`, where the enum is backed.
#[must_use]
pub fn enum_methods(backing: EnumBacking) -> Vec<MethodDecl> {
    let value = || parameter(b"value", Some(Type::Union(vec![Type::Int, Type::String])));
    let mut out = vec![method(b"cases", Vec::new(), Type::Array)];
    if backing != EnumBacking::Pure {
        out.push(method(b"from", vec![value()], Type::CalledClass));
        out.push(method(
            b"tryFrom",
            vec![value()],
            Type::Nullable(Box::new(Type::CalledClass)),
        ));
    }
    out
}

/// How many rows [`enum_methods`] gives an enum of `backing`: `cases`,
/// and `from` and `tryFrom` where it is backed.
fn given(backing: EnumBacking) -> usize {
    match backing {
        EnumBacking::Pure => 1,
        EnumBacking::Int | EnumBacking::String => 3,
    }
}

/// The methods `class`'s declaration writes: every row of its
/// `methods` but, for an enum, the [`enum_methods`] after them, which
/// php registers as the enum links (`zend_enum_register_funcs`) as
/// internal functions under its known names (`ZSTR_KNOWN`), so no
/// compile makes a string or an op array for one.
#[must_use]
pub fn written_methods(class: &ClassDecl) -> &[MethodDecl] {
    if class.kind != ClassKind::Enum {
        return &class.methods;
    }
    let written = class.methods.len().saturating_sub(given(class.backing));
    class.methods.get(..written).unwrap_or_default()
}

fn method(name: &[u8], parameters: Vec<Parameter>, returns: Type) -> MethodDecl {
    MethodDecl {
        function: FunctionDecl {
            parameters,
            return_type: Some(returns),
            ..native_function(name)
        },
        visibility: Visibility::Public,
        is_instance: false,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }
}
