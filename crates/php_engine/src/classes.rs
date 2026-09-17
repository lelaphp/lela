//! The classes Zend itself declares (`zend_exceptions.c`), which every
//! engine has whether or not an extension is bundled: the throwable
//! hierarchy the raisers of `api::raising` name. Declared here as an
//! extension's classes are declared, so the linker that arrives in week
//! two (design §11) reads one model and not two; until it does, what a
//! declaration gives is the class a throwable knows itself by, which is
//! the name an uncaught fatal prints.

use php_unit::decl::{
    ClassDecl, ClassFlags, ClassKind, EnumBacking, FunctionDecl, MethodDecl, Name, Visibility,
};

use crate::api::abi::{Entry, NativeClassDecl};
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::Value;

/// `Throwable` and the throwables php declares under it, parents first,
/// each with the methods it declares so far beside it: `Exception` and
/// `Error` carry their private `__clone` (`zend_exceptions.stub.php`),
/// the one method of theirs the visibility rule of
/// `zend_std_get_method` has to be held to. The rest of their methods
/// arrive with the linker (design §6.2).
pub(crate) fn zend_classes() -> Vec<NativeClassDecl> {
    vec![
        (
            declared(b"Stringable", ClassKind::Interface, None),
            Vec::new(),
        ),
        (
            declared(b"Throwable", ClassKind::Interface, None),
            Vec::new(),
        ),
        (
            declared(b"Exception", ClassKind::Class, None),
            vec![(private_clone(), throwable_clone as Entry)],
        ),
        (
            declared(b"Error", ClassKind::Class, None),
            vec![(private_clone(), throwable_clone as Entry)],
        ),
        (
            declared(b"TypeError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
        ),
        (
            declared(b"ValueError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
        ),
        (
            declared(b"ArithmeticError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
        ),
        (
            declared(
                b"DivisionByZeroError",
                ClassKind::Class,
                Some(b"ArithmeticError"),
            ),
            Vec::new(),
        ),
        (
            declared(b"ArgumentCountError", ClassKind::Class, Some(b"TypeError")),
            Vec::new(),
        ),
        (
            declared(b"UnhandledMatchError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
        ),
        (
            declared(b"CompileError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
        ),
        (
            declared(b"ParseError", ClassKind::Class, Some(b"CompileError")),
            Vec::new(),
        ),
    ]
}

/// `private function __clone(): void {}`, as `Exception` and `Error`
/// declare it.
fn private_clone() -> MethodDecl {
    MethodDecl {
        function: FunctionDecl {
            name: Name(b"__clone".as_slice().into()),
            parameters: Vec::new(),
            return_type: Some(php_unit::decl::Type::Void),
            returns_by_ref: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            frameless: Vec::new(),
        },
        visibility: Visibility::Private,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }
}

/// `ZEND_METHOD(Exception, __clone)`: private, so a call from outside
/// the class never reaches it; what reflection could reach is php's
/// `Exception` saying an object is not cloned this way.
fn throwable_clone(req: &mut Request) -> Result<Value, Thrown> {
    Err(crate::api::raising::throw(
        req,
        "Exception",
        "Cannot clone object using __clone()",
    ))
}

fn declared(name: &[u8], kind: ClassKind, parent: Option<&[u8]>) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind,
        flags: ClassFlags::default(),
        parent: parent.map(|p| Name(p.into())),
        // php declares the interfaces on `Exception` and `Error` alone
        // and the rest inherit them (`zend_register_default_classes`);
        // the linker is what carries them down (design §6.2).
        interfaces: match (kind, parent) {
            (ClassKind::Class, None) => vec![
                Name(b"Throwable".as_slice().into()),
                Name(b"Stringable".as_slice().into()),
            ],
            _ => Vec::new(),
        },
        traits: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        properties: Vec::new(),
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
    }
}
