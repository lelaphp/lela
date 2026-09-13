//! The schema held to its reader and its emitter: `php.ungram` parses,
//! validates, declares the 19 token classes and no other, and emits the
//! kinds `lib.rs` includes. The floors sit just below the counts read at
//! the pin (139 struct nodes, 72 keywords, 66 puncts including the implied
//! `COMMA`, 19 token classes), so they catch a reader that drops half the
//! schema without churning on a legitimate addition. The generator is
//! included by path, as `build.rs` includes it, which is also what runs
//! its own unit tests under `cargo test`.

use std::path::Path;

// The same body `build.rs` includes; what this test does not call is not
// dead.
#[allow(dead_code)]
#[path = "../codegen/mod.rs"]
mod codegen;

use codegen::kind;
use codegen::ungram::{self, DefClass, classify};

fn real_grammar_text() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("php.ungram");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

#[test]
fn real_grammar_parses_validates_and_generates() {
    let text = real_grammar_text();
    let grammar = match ungram::parse(&text) {
        Ok(grammar) => grammar,
        Err(errors) => panic!("php.ungram failed to parse: {errors:#?}"),
    };

    let validation = ungram::validate(&grammar);
    assert!(validation.is_empty(), "validation errors: {validation:#?}");

    let class_errors = kind::check_token_classes(&grammar, kind::TOKEN_CLASSES);
    assert!(
        class_errors.is_empty(),
        "token-class cross-check errors: {class_errors:#?}"
    );

    // Sanity floors (see module docs for the observed counts).
    let counts = kind::kind_counts(&grammar).expect("kind collection succeeds");
    assert!(counts.nodes > 120, "struct nodes: {}", counts.nodes);
    assert!(counts.keywords > 60, "keywords: {}", counts.keywords);
    assert!(counts.puncts > 55, "puncts: {}", counts.puncts);
    assert_eq!(
        counts.classes, 19,
        "token classes are an explicit, cross-checked list"
    );
    assert_eq!(counts.extras, 9, "extras are a fixed list");

    // Convention A1 spot checks: enums and token-set aliases exist and do
    // not become struct nodes.
    let class_of = |name: &str| {
        let def = grammar
            .defs
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("definition `{name}` missing"));
        classify(def)
    };
    assert_eq!(class_of("Expr"), DefClass::Enum);
    assert_eq!(class_of("Stmt"), DefClass::Enum);
    assert_eq!(class_of("Modifier"), DefClass::Enum, "mixed alternation");
    assert_eq!(
        class_of("ScopedSelector"),
        DefClass::Enum,
        "mixed alternation with a token variant"
    );
    assert_eq!(class_of("BinaryOp"), DefClass::TokenAlt);
    assert_eq!(class_of("NameRef"), DefClass::TokenAlt);
    assert_eq!(class_of("ClassDecl"), DefClass::StructNode);
    assert_eq!(class_of("HasDocComment"), DefClass::Trait);

    // The empty-bodied trait parsed as such.
    let has_doc = grammar
        .defs
        .iter()
        .find(|d| d.name == "HasDocComment")
        .expect("HasDocComment exists");
    assert!(has_doc.is_trait && has_doc.rule.is_none());

    // Mixin opt-ins survived.
    let class_decl = grammar
        .defs
        .iter()
        .find(|d| d.name == "ClassDecl")
        .expect("ClassDecl exists");
    assert_eq!(
        class_decl.traits,
        ["HasAttributes", "HasModifiers", "HasDocComment"]
    );

    // Trait count: exactly the three mixins the schema declares today.
    let traits = grammar.defs.iter().filter(|d| d.is_trait).count();
    assert_eq!(traits, 3);

    // Generated output spot checks.
    let generated = kind::generate(&grammar).expect("generation succeeds");
    for needle in [
        "never committed",
        "#[repr(u16)]",
        "pub enum SyntaxKind",
        "CLASS_DECL,",
        "FUNCTION_KW,",
        "HALT_COMPILER_KW,",
        "SEMICOLON,",
        "ATTRIBUTE_OPEN,",
        "COMMA,",
        "IDENT,",
        "VARIABLE,",
        "PIPE_GT,",
        "WHITESPACE,",
        "EOF,",
        "pub fn from_keyword(ident: &str) -> Option<Self>",
    ] {
        assert!(
            generated.contains(needle),
            "generated output missing {needle:?}"
        );
    }
    // Enums and token aliases must NOT produce kinds (convention A1).
    for absent in ["    EXPR,", "    STMT,", "    BINARY_OP,", "    NAME_REF,"] {
        assert!(
            !generated.contains(absent),
            "alternation-only definition leaked a SyntaxKind: {absent:?}"
        );
    }
}

/// What `build.rs` wrote is what `lib.rs` includes: the enum is there, and
/// its predicates and `from_keyword` answer as the schema says.
#[test]
fn the_included_enum_answers_for_the_schema() {
    use php_syntax::SyntaxKind;
    assert!(SyntaxKind::FUNCTION_KW.is_keyword());
    assert!(SyntaxKind::SEMICOLON.is_punct());
    assert!(SyntaxKind::WHITESPACE.is_trivia());
    assert!(SyntaxKind::DOC_COMMENT.is_trivia());
    assert!(!SyntaxKind::OPEN_TAG.is_trivia());
    assert!(!SyntaxKind::EOF.is_trivia());
    assert!(!SyntaxKind::CLASS_DECL.is_keyword());
    assert!(!SyntaxKind::CLASS_DECL.is_punct());
    assert_eq!(
        SyntaxKind::from_keyword("FUNCTION"),
        Some(SyntaxKind::FUNCTION_KW)
    );
    assert_eq!(
        SyntaxKind::from_keyword("__halt_compiler"),
        Some(SyntaxKind::HALT_COMPILER_KW)
    );
    assert_eq!(SyntaxKind::from_keyword("foo"), None);
}
