//! The typed nodes on hand-built trees, at the spots the generator and the
//! builder can silently get wrong: anchored same-typed siblings (convention
//! A4), token-backed enum variants (A1), required children (A2), the mixin
//! traits (A5) and the doc comment the builder takes (php's
//! `CG(doc_comment)`). A tree is built as the parser builds one, from the
//! lexer's tokens and a hand-written event stream, so what is tested is the
//! tree the engine will walk and the dump every fixture renders through.

mod support;

use php_syntax::tree::{
    self, Built, ClassDecl, Element, Event, Expr, ExprStmt, HasAttributes, HasDocComment,
    LiteralExpr, MemberAccessExpr, MemberSelector, Node, SimpleVarExpr, TOMBSTONE, TernaryExpr,
};
use php_syntax::{FileId, LexOptions, SyntaxKind, Token, lex};

/// One hand-built tree with the source and the tokens it was built over.
struct Hand {
    source: &'static [u8],
    tokens: Vec<Token>,
    built: Built<'static>,
}

impl Hand {
    fn new(source: &'static [u8], events: Vec<Event>) -> Hand {
        let tokens = lex(FileId(0), source, &LexOptions::default()).tokens;
        let arena: &'static php_syntax::Arena = Box::leak(Box::new(php_syntax::Arena::new()));
        let built = tree::build(
            arena,
            &tokens,
            &php_syntax::LineIndex::new(arena, source),
            events,
        );
        Hand {
            source,
            tokens,
            built,
        }
    }

    fn dump(&self) -> String {
        tree::dump(&self.built.tree, &self.tokens, self.source)
    }

    fn text(&self, id: tree::TokenId) -> &[u8] {
        let token = self.tokens[id.0 as usize];
        &self.source[token.offset as usize..(token.offset + token.len) as usize]
    }

    fn root<T: Node>(&self) -> &T {
        let id = self
            .built
            .tree
            .root()
            .and_then(Element::id::<T>)
            .unwrap_or_else(|| panic!("the root is a {}", std::any::type_name::<T>()));
        &self.built.tree[id]
    }
}

fn start(kind: SyntaxKind) -> Event {
    Event::Start {
        kind,
        forward_parent: None,
    }
}

fn token(kind: SyntaxKind) -> Event {
    Event::Token { remap: kind }
}

const FINISH: Event = Event::Finish;

/// `$x ? 1 : 2`, as `TERNARY_EXPR(SIMPLE_VAR_EXPR ? LITERAL_EXPR : LITERAL_EXPR)`
/// after the open tag, which the root takes.
fn full_ternary() -> Hand {
    use SyntaxKind::*;
    Hand::new(
        b"<?php $x ? 1 : 2",
        vec![
            start(TERNARY_EXPR),
            token(OPEN_TAG),
            start(SIMPLE_VAR_EXPR),
            token(VARIABLE),
            FINISH,
            token(QUESTION),
            start(LITERAL_EXPR),
            token(INT),
            FINISH,
            token(COLON),
            start(LITERAL_EXPR),
            token(INT),
            FINISH,
            FINISH,
        ],
    )
}

/// `$x ?: 2`: the short ternary, `then` absent.
fn short_ternary() -> Hand {
    use SyntaxKind::*;
    Hand::new(
        b"<?php $x ?: 2",
        vec![
            start(TERNARY_EXPR),
            token(OPEN_TAG),
            start(SIMPLE_VAR_EXPR),
            token(VARIABLE),
            FINISH,
            token(QUESTION),
            token(COLON),
            start(LITERAL_EXPR),
            token(INT),
            FINISH,
            FINISH,
        ],
    )
}

#[test]
fn ternary_anchored_fields() {
    let hand = full_ternary();
    let node: &TernaryExpr = hand.root();
    assert!(matches!(node.cond, Expr::SimpleVarExpr(_)));
    let Some(Expr::LiteralExpr(then)) = node.then else {
        panic!("a full ternary has a then: {:?}", node.then);
    };
    assert_eq!(hand.built.tree[then].range.start, 11);
    let Some(Expr::LiteralExpr(else_branch)) = node.else_branch else {
        panic!("has an else: {:?}", node.else_branch);
    };
    assert_eq!(hand.built.tree[else_branch].range.start, 15);
    assert_eq!(node.range.end(), 16);
    assert!(hand.built.missing.is_empty());
}

#[test]
fn short_ternary_then_is_none() {
    let hand = short_ternary();
    let node: &TernaryExpr = hand.root();
    // A4: `then` is the expression BETWEEN `?` and `:`, absent here, and
    // it must not leak the else branch.
    assert!(node.then.is_none());
    assert!(node.else_branch.is_some());
}

#[test]
fn member_access_member_is_anchored() {
    // `$a->$b`: obj and member are both SimpleVarExpr; member resolves
    // after the arrow, not to the first variable.
    use SyntaxKind::*;
    let hand = Hand::new(
        b"<?php $a->$b",
        vec![
            start(MEMBER_ACCESS_EXPR),
            token(OPEN_TAG),
            start(SIMPLE_VAR_EXPR),
            token(VARIABLE),
            FINISH,
            token(ARROW),
            start(SIMPLE_VAR_EXPR),
            token(VARIABLE),
            FINISH,
            FINISH,
        ],
    );
    let node: &MemberAccessExpr = hand.root();
    let Expr::SimpleVarExpr(obj) = node.obj else {
        panic!("obj is the first variable: {:?}", node.obj);
    };
    assert_eq!(hand.built.tree[obj].range.start, 6);
    let Some(MemberSelector::SimpleVarExpr(member)) = node.member else {
        panic!("member is the variable after the arrow: {:?}", node.member);
    };
    assert_eq!(hand.built.tree[member].range.start, 10);
    assert_eq!(hand.text(node.op.expect("the arrow")), b"->");
}

#[test]
fn bare_name_ref_is_an_expr() {
    // `FOO;`, a constant fetch: the IDENT token sits directly in EXPR_STMT.
    use SyntaxKind::*;
    let hand = Hand::new(
        b"<?php FOO;",
        vec![
            start(EXPR_STMT),
            token(OPEN_TAG),
            token(IDENT),
            token(SEMICOLON),
            FINISH,
        ],
    );
    let stmt: &ExprStmt = hand.root();
    match stmt.expr {
        Expr::NameRef(id) => assert_eq!(hand.text(id), b"FOO"),
        other => panic!("expected NameRef, got {other:?}"),
    }
}

#[test]
fn a_required_child_absent_is_reported_and_the_node_is_an_error() {
    // An EXPR_STMT with no expression breaks `ExprStmt.expr!`: the node
    // is not built, the shape holds it as ERROR with its children, and
    // `missing` names the field.
    use SyntaxKind::*;
    let hand = Hand::new(
        b"<?php ;",
        vec![
            start(SOURCE_FILE),
            token(OPEN_TAG),
            start(EXPR_STMT),
            token(SEMICOLON),
            FINISH,
            FINISH,
        ],
    );
    assert_eq!(hand.built.missing.len(), 1, "{:?}", hand.built.missing);
    assert_eq!(hand.built.missing[0].field, "ExprStmt.expr");
    assert_eq!(hand.built.missing[0].range.start, 6);
    assert!(hand.built.tree.nodes::<ExprStmt>().is_empty());
    assert_eq!(
        hand.dump(),
        "SOURCE_FILE@0..7\n  OPEN_TAG@0..6 \"<?php \"\n  ERROR@6..7\n    SEMICOLON@6..7 \";\"\n"
    );
    // The well-formed trees above have none.
    assert!(full_ternary().built.missing.is_empty());
}

#[test]
fn mixins_and_doc_comments() {
    // `/** Doc */ #[Attr] class C {}`: the doc comment is taken by the
    // attribute group at `#[`, pending again after `]` and taken by the
    // declaration before its `{`, as the grammar places the events; the
    // attribute group is the declaration's first field, and the name
    // field skips the `class` keyword to `C`.
    use SyntaxKind::*;
    let hand = Hand::new(
        b"<?php /** Doc */ #[Attr] class C {}",
        vec![
            start(SOURCE_FILE),
            token(OPEN_TAG),
            start(CLASS_DECL),
            start(ATTRIBUTE_GROUP),
            Event::DocComment,
            token(ATTRIBUTE_OPEN),
            start(ATTRIBUTE),
            token(IDENT),
            FINISH,
            token(R_BRACK),
            Event::DocCommentRestore,
            FINISH,
            token(CLASS_KW),
            token(IDENT),
            Event::DocComment,
            start(CLASS_BODY),
            token(L_CURLY),
            token(R_CURLY),
            FINISH,
            FINISH,
            FINISH,
        ],
    );
    assert!(hand.built.missing.is_empty(), "{:?}", hand.built.missing);
    let class = &hand.built.tree.nodes::<ClassDecl>()[0];
    assert_eq!(class.attrs().len(), 1);
    let doc = class.doc_comment().expect("the doc comment is taken");
    assert_eq!(hand.text(doc), b"/** Doc */");
    assert_eq!(hand.text(class.name.expect("named class")), b"C");
    assert_eq!(hand.built.tree[class.body].range.start, 33);
    assert_eq!(class.range.start, 17, "the range starts at the attribute");
}

#[test]
fn the_shape_walks_every_node_and_significant_token_in_order() {
    // What the visitor did: a count by kind is a pass over the shape,
    // and the dump is the same pass with the text beside each token.
    let hand = full_ternary();
    let elements = hand.built.tree.elements();
    let count = |kind: SyntaxKind| elements.iter().filter(|e| e.kind() == kind).count();
    assert_eq!(count(SyntaxKind::TERNARY_EXPR), 1);
    assert_eq!(count(SyntaxKind::LITERAL_EXPR), 2);
    assert_eq!(
        count(SyntaxKind::WHITESPACE),
        0,
        "trivia is not in the tree"
    );
    assert_eq!(hand.built.tree.nodes::<LiteralExpr>().len(), 2);
    assert_eq!(hand.built.tree.nodes::<SimpleVarExpr>().len(), 1);
    assert_eq!(
        hand.dump(),
        "TERNARY_EXPR@0..16\n\
         \x20 OPEN_TAG@0..6 \"<?php \"\n\
         \x20 SIMPLE_VAR_EXPR@6..8\n\
         \x20   VARIABLE@6..8 \"$x\"\n\
         \x20 QUESTION@9..10 \"?\"\n\
         \x20 LITERAL_EXPR@11..12\n\
         \x20   INT@11..12 \"1\"\n\
         \x20 COLON@13..14 \":\"\n\
         \x20 LITERAL_EXPR@15..16\n\
         \x20   INT@15..16 \"2\"\n"
    );
}

#[test]
fn a_remapped_keyword_is_ident_in_the_tree_and_a_keyword_in_the_stream() {
    // `Foo::list`: the lexer says LIST_KW, which `token_get_all` reports
    // as php does (T_LIST); the parser consumes it as a name and the tree
    // reads IDENT.
    use SyntaxKind::*;
    let hand = Hand::new(
        b"<?php Foo::list",
        vec![
            start(SOURCE_FILE),
            token(OPEN_TAG),
            token(IDENT),
            token(COLON_COLON),
            Event::Token { remap: IDENT },
            FINISH,
        ],
    );
    let last = hand.built.tree.elements().last().copied();
    let Some(Element::Token { kind, id }) = last else {
        panic!("{last:?}");
    };
    assert_eq!(kind, IDENT);
    assert_eq!(hand.tokens[id.0 as usize].kind, LIST_KW);
    assert!(hand.dump().ends_with("  IDENT@11..15 \"list\"\n"));
}

#[test]
fn a_forward_parent_wraps_the_node_before_it() {
    // `$x + 1` as the Pratt core emits it: the lhs starts first and is
    // preceded by the BINARY_EXPR whose Start comes later in the stream.
    use SyntaxKind::*;
    let hand = Hand::new(
        b"<?php $x + 1",
        vec![
            start(SOURCE_FILE),
            token(OPEN_TAG),
            Event::Start {
                kind: SIMPLE_VAR_EXPR,
                forward_parent: Some(2),
            },
            token(VARIABLE),
            start(BINARY_EXPR),
            FINISH,
            start(TOMBSTONE),
            token(PLUS),
            start(LITERAL_EXPR),
            token(INT),
            FINISH,
            FINISH,
            FINISH,
        ],
    );
    assert!(hand.built.missing.is_empty(), "{}", hand.dump());
    assert_eq!(
        hand.dump(),
        "SOURCE_FILE@0..12\n\
         \x20 OPEN_TAG@0..6 \"<?php \"\n\
         \x20 BINARY_EXPR@6..12\n\
         \x20   SIMPLE_VAR_EXPR@6..8\n\
         \x20     VARIABLE@6..8 \"$x\"\n\
         \x20   PLUS@9..10 \"+\"\n\
         \x20   LITERAL_EXPR@11..12\n\
         \x20     INT@11..12 \"1\"\n"
    );
}

#[test]
fn display_names_cover_token_kinds() {
    assert_eq!(SyntaxKind::SEMICOLON.display_name(), Some("`;`"));
    assert_eq!(SyntaxKind::MATCH_KW.display_name(), Some("`match`"));
    assert_eq!(SyntaxKind::IDENT.display_name(), Some("an identifier"));
    assert_eq!(SyntaxKind::CLOSE_TAG.display_name(), Some("`?>`"));
    // Node kinds have no display name: messages never speak kind names.
    assert_eq!(SyntaxKind::CLASS_DECL.display_name(), None);
}

/// Losslessness: the token texts of a parse concatenate to the input, on
/// every shape the lexer has a state for and on what it refuses.
#[test]
fn the_token_texts_concatenate_to_the_input() {
    for source in [
        &b""[..],
        b"<html>",
        b"<?php echo 1;",
        b"<?php /** d */ class C { public $x = \"a{$b[1]}c\"; }\n?>\ntail",
        b"<?php $s = <<<EOT\n  in $x\n  EOT;\n",
        b"<?php __halt_compiler(); raw \0 bytes",
        b"<?php /* unterminated",
        b"<?= 'x' ?>\n<?php 0x1G;",
        b"\xff\xfe<?php \"\xc3\x28\";",
    ] {
        let parsed = support::parse_lossless(source);
        // One root over every significant token until the grammar lands.
        let significant = parsed.tokens.iter().filter(|t| !t.kind.is_trivia()).count();
        let in_tree = parsed
            .tree
            .elements()
            .iter()
            .filter(|e| matches!(e, Element::Token { .. }))
            .count();
        assert_eq!(in_tree, significant, "{}", String::from_utf8_lossy(source));
        assert!(
            parsed.dump().starts_with("SOURCE_FILE@"),
            "{}",
            parsed.dump()
        );
    }
}
