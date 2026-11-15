//! The schema held to its reader and its emitters: `php.ungram` parses,
//! validates, declares the 19 token classes and no other, and emits the
//! kinds `lib.rs` includes and the typed nodes `tree.rs` includes, one
//! struct per node kind and no accessor that unwraps. The floors sit just below the counts read at
//! the pin (139 struct nodes, 72 keywords, 66 puncts including the implied
//! `COMMA`, 19 token classes), so they catch a reader that drops half the
//! schema without churning on a legitimate addition. The generator is
//! included by path, as `build.rs` includes it, which is also what runs
//! its own unit tests under `cargo test`.

use std::path::PathBuf;

// The same body `build.rs` includes; what this test does not call is not
// dead.
#[allow(dead_code)]
#[path = "../codegen/mod.rs"]
mod codegen;

use codegen::kind;
use codegen::ungram::{self, DefClass, classify};

/// This crate's directory as cargo sets `CARGO_MANIFEST_DIR` when it runs
/// the test, never the path `env!` compiled in: cargo reuses a test binary
/// across two checkouts that share a target directory.
fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .expect("cargo sets CARGO_MANIFEST_DIR for a test it runs")
}

fn real_grammar_text() -> String {
    let path = manifest_dir().join("php.ungram");
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
    assert_eq!(counts.extras, 10, "extras are a fixed list");

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
        "pub fn from_keyword(ident: &[u8]) -> Option<Self>",
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
        SyntaxKind::from_keyword(b"FUNCTION"),
        Some(SyntaxKind::FUNCTION_KW)
    );
    assert_eq!(
        SyntaxKind::from_keyword(b"__halt_compiler"),
        Some(SyntaxKind::HALT_COMPILER_KW)
    );
    assert_eq!(SyntaxKind::from_keyword(b"foo"), None);
}

/// The typed nodes `build.rs` wrote and `tree.rs` includes, read back as
/// text: one struct per struct node of the schema, a `!` child a plain
/// field, no `expect(` or `unwrap(` behind any read, and no `Visitor`
/// (ADR-0003, design §4.9).
#[test]
fn the_emitted_tree_is_one_struct_per_node_kind_and_unwraps_nothing() {
    let emitted = include_str!(concat!(env!("OUT_DIR"), "/tree.rs"));
    let grammar = ungram::parse(&real_grammar_text()).expect("php.ungram parses");
    let structs = emitted.matches("\npub struct ").count();
    assert_eq!(
        structs,
        grammar.struct_nodes().len() + 2,
        "one struct per struct node, the arena and its lists"
    );
    let resolvers = emitted.matches("\n    fn resolve(\n").count();
    assert_eq!(
        resolvers,
        grammar.struct_nodes().len(),
        "one resolver per struct node"
    );
    assert!(emitted.contains("\npub struct Nodes<'a> {\n"));
    assert!(emitted.contains("\npub struct Lists<'a> {\n"));
    // A list the schema writes head-and-tail is one field, not a head
    // beside a tail.
    for absent in [
        "expect(",
        "unwrap(",
        "Visitor",
        "fn visit",
        "SyntaxNode",
        "pub type_expr",
        "pub name_ref",
    ] {
        assert!(
            !emitted.contains(absent),
            "emitted tree.rs holds {absent:?}"
        );
    }
    // `!` children are the bare type; everything else is `Option` or a list.
    for needle in [
        "    pub lhs: Expr,\n",
        "    pub op: Option<TokenId>,\n",
        "    pub body: NodeId<ClassBody>,\n",
        "    pub name: TokenId,\n",
        "    pub params: NodeId<ParamList>,\n",
        "    pub stmts: List<Stmt>,\n",
        "    pub modifiers: List<Modifier>,\n",
        "    pub doc_comment: Option<TokenId>,\n",
        "    pub types: List<TypeExpr>,\n",
        "    pub types: List<TokenId>,\n",
        "    pub neg: Option<TokenId>,\n",
        "pub enum Expr {\n",
        "    NameRef(TokenId),\n",
        "    ClassKw(TokenId),\n",
        "pub trait HasAttributes {\n",
        "pub fn is_binary_op(self) -> bool",
    ] {
        assert!(
            emitted.contains(needle),
            "emitted tree.rs missing {needle:?}"
        );
    }
}

/// The emitted nodes are what the crate exports: a node is built from
/// its fields, its kind is its struct's, and an alternation says which
/// node stands there without a cast.
#[test]
fn the_included_nodes_are_built_from_fields() {
    use php_syntax::tree::{BinaryExpr, Expr, HasModifiers, List, Node, NodeId, Param, TokenId};
    use php_syntax::{SyntaxKind, TextRange};

    let sum = BinaryExpr {
        range: TextRange::new(6, 12),
        line: 2,
        lhs: Expr::SimpleVarExpr(NodeId::new(0)),
        op: Some(TokenId(3)),
        rhs: Some(Expr::LiteralExpr(NodeId::new(1))),
    };
    assert_eq!(BinaryExpr::KIND, SyntaxKind::BINARY_EXPR);
    assert_eq!(sum.range().end(), 12);
    assert_eq!(sum.line(), 2);
    assert!(matches!(sum.lhs, Expr::SimpleVarExpr(id) if id.index() == 0));

    let param = Param {
        range: TextRange::new(0, 10),
        line: 1,
        attrs: List::default(),
        modifiers: List::new(4, 2),
        doc_comment: None,
        ty: None,
        byref: None,
        variadic: None,
        var: TokenId(2),
        default: None,
        hooks: None,
    };
    assert_eq!(param.modifiers().len(), 2);
    assert!(SyntaxKind::PLUS.is_binary_op());
    assert!(!SyntaxKind::PLUS.is_name_ref());
    assert_eq!(SyntaxKind::PLUS.display_name(), Some("`+`"));
    assert_eq!(SyntaxKind::BINARY_EXPR.display_name(), None);
}
