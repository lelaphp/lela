//! The classes Zend itself declares (`zend_exceptions.c`), which every
//! engine has whether or not an extension is bundled: the throwable
//! hierarchy the raisers of `api::raising` name. Declared here as an
//! extension's classes are declared, so the linker that arrives in week
//! two (design §11) reads one model and not two; until it does, what a
//! declaration gives is the class a throwable knows itself by, which is
//! the name an uncaught fatal prints.

use php_unit::decl::{ClassDecl, ClassFlags, ClassKind, EnumBacking, Name};

/// `Throwable` and the throwables php declares under it, parents first.
pub(crate) fn zend_classes() -> Vec<ClassDecl> {
    vec![
        declared(b"Stringable", ClassKind::Interface, None),
        declared(b"Throwable", ClassKind::Interface, None),
        declared(b"Exception", ClassKind::Class, None),
        declared(b"Error", ClassKind::Class, None),
        declared(b"TypeError", ClassKind::Class, Some(b"Error")),
        declared(b"ValueError", ClassKind::Class, Some(b"Error")),
        declared(b"ArithmeticError", ClassKind::Class, Some(b"Error")),
        declared(
            b"DivisionByZeroError",
            ClassKind::Class,
            Some(b"ArithmeticError"),
        ),
        declared(b"ArgumentCountError", ClassKind::Class, Some(b"TypeError")),
        declared(b"UnhandledMatchError", ClassKind::Class, Some(b"Error")),
        declared(b"CompileError", ClassKind::Class, Some(b"Error")),
        declared(b"ParseError", ClassKind::Class, Some(b"CompileError")),
    ]
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
