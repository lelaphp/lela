//! `link` through its public signature, over a table of declarations:
//! one refusal at each stage `zend_do_link_class` walks, in its order
//! (an enum's functions, the traits' adaptations, the parent, the
//! interfaces), each its sentence at the class's line, and one class
//! with a parent and an interface that links.

use php_link::{Held, Linked, Refused, link};
use php_unit::decl::{
    ClassDecl, ClassFlags, ClassKind, ConstExpr, EnumBacking, MethodDecl, Name, TraitAdaptation,
    Visibility, native_function,
};
use php_unit::enum_methods;

/// A declaration named `name` of `kind` at line 1 of `t.php`, with
/// nothing in it: `ClassDecl` has no `Default`.
fn class(name: &str, kind: ClassKind) -> ClassDecl {
    ClassDecl {
        name: Name::from(name),
        kind,
        flags: ClassFlags::default(),
        parent: None,
        interfaces: Vec::new(),
        implementors: Vec::new(),
        aliases: Vec::new(),
        traits: Vec::new(),
        adaptations: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        properties: Vec::new(),
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name::from("t.php"),
        line: 1,
        end_line: 1,
        object_size: 0,
        object_beside: Box::default(),
        attribute_validator: None,
    }
}

/// A public instance method named `name` with a body.
fn method(name: &[u8]) -> MethodDecl {
    MethodDecl {
        function: native_function(name),
        visibility: Visibility::Public,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }
}

/// Every class the cases link against and link, each linked.
fn table() -> Vec<ClassDecl> {
    let mut parent = class("P", ClassKind::Class);
    parent.methods.push(method(b"m"));
    let mut final_parent = class("F", ClassKind::Class);
    final_parent.flags.is_final = true;
    let mut suit = class("Suit", ClassKind::Enum);
    suit.line = 2;
    suit.methods.push(method(b"Cases"));
    suit.methods.extend(enum_methods(EnumBacking::Pure));
    let mut extends_final = class("ExtendsFinal", ClassKind::Class);
    extends_final.line = 4;
    extends_final.parent = Some(Name::from("F"));
    let mut adapts = class("Adapts", ClassKind::Class);
    adapts.line = 3;
    adapts.traits.push(Name::from("T"));
    adapts.adaptations.push(TraitAdaptation::Insteadof {
        trait_name: Name::from("T"),
        method: Name::from("missing"),
        excluded: vec![Name::from("T")],
    });
    let mut implements_class = class("ImplementsClass", ClassKind::Class);
    implements_class.line = 5;
    implements_class.interfaces.push(Name::from("P"));
    let mut links = class("Links", ClassKind::Class);
    links.parent = Some(Name::from("P"));
    links.interfaces.push(Name::from("I"));
    links.methods.push(method(b"m"));
    vec![
        parent,
        final_parent,
        class("I", ClassKind::Interface),
        class("T", ClassKind::Trait),
        suit,
        extends_final,
        adapts,
        implements_class,
        links,
    ]
}

/// What `link` came to for the class of `table` named `name`, the
/// table its lookup, by name in any case, and the line the class is
/// declared at.
fn outcome(table: &[ClassDecl], name: &str) -> (Linked, u32) {
    let lookup = |wanted: &[u8]| {
        table
            .iter()
            .find(|decl| decl.name.0.eq_ignore_ascii_case(wanted))
            .map(|decl| Held {
                decl,
                bodies: None,
                linked: true,
                record: None,
            })
    };
    let holder = |(): ()| -> Option<&ClassDecl> { None };
    let values = |_: &ConstExpr| -> Option<ConstExpr> { None };
    let child = table
        .iter()
        .find(|decl| *decl.name.0 == *name.as_bytes())
        .expect("the table holds the class");
    (link(child, &lookup, &holder, &values).outcome, child.line)
}

#[test]
fn each_stage_refuses_with_its_sentence_at_the_class() {
    let table = table();
    for (name, sentence) in [
        ("Suit", "Cannot redeclare Suit::cases()"),
        (
            "Adapts",
            "A precedence rule was defined for T::missing but this method does not exist",
        ),
        (
            "ExtendsFinal",
            "Class ExtendsFinal cannot extend final class F",
        ),
        (
            "ImplementsClass",
            "ImplementsClass cannot implement P - it is not an interface",
        ),
    ] {
        let (linked, line) = outcome(&table, name);
        let refused = match linked {
            Linked::Refused(Refused { refusal, site }) => Some((refusal.to_string(), site.line)),
            Linked::Linked => None,
        };
        assert_eq!(refused, Some((sentence.to_owned(), line)), "{name}");
    }
}

#[test]
fn a_class_with_a_parent_and_an_interface_links() {
    assert_eq!(outcome(&table(), "Links").0, Linked::Linked);
}
