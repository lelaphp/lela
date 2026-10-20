//! The tree builder (design §4.9, ADR-0003): the parser's flat event
//! stream into the arena and the shape, in one pass. The parser emits
//! [`Event`]s and never sees a node; the builder opens a frame per `Start`,
//! appends each consumed token to the innermost frame, and at `Finish`
//! resolves the frame's children into the typed struct of its kind, once,
//! through the emitted `resolve`. Nothing scans afterwards.
//!
//! **What the tree references.** Significant tokens only: whitespace,
//! comments and heredoc indentation stay in the token stream, and a
//! `Token` event consumes the next significant token, so the builder passes
//! over what sits before it. Tokens the parser never consumed, which
//! recovery can leave at the end, are appended to the root as its last
//! children, so the shape shows every significant token once.
//!
//! **The doc comment.** php keeps one pending doc comment, `CG(doc_comment)`:
//! the last `T_DOC_COMMENT` the scanner passed, cleared by every `}`
//! (`zend_language_scanner.l`, the `<ST_IN_SCRIPTING>"}"` rule at 8.5.9),
//! and taken, then cleared, where the parser reduces `backup_doc_comment`
//! in `zend_language_parser.y`. So a blank line does not detach a doc
//! comment, a statement between the comment and the declaration does not
//! either, and a doc comment before a closure's `}` is gone by the next
//! declaration. Measured under the pin at 8.5.9 with
//! `ReflectionClass::getDocComment()`: `/** D1 */\n\nclass A {}` reads D1,
//! `/** D2 */ $x = 1; class B {}` reads D2, `function f() {} /** D3 */\n
//! class C {}` reads D3, and `/** D7 */ if (1) { } class G {}` reads
//! `false`. Mahout's rule, the comment block directly above with no blank
//! line, is not php's and is not kept.
//!
//! The builder walks the same state over the token stream, and the take is
//! the grammar's: the parser emits [`Event::DocComment`] where php's rule
//! has its `backup_doc_comment`, and the innermost open node takes what is
//! pending then. What is pending is what php's scanner has passed, and
//! bison reads a token ahead only where a state needs the lookahead to
//! choose: a `backup_doc_comment` reduced in a state with that one action
//! is reduced before the next token is scanned. So the grammar places the
//! event before the last token php had read at the reduction, and the
//! builder passes over the whitespace and comments before that token and
//! takes: a class takes before its `{`, since `extends_from` and
//! `implements_list` need the lookahead (`class F extends /** D5 */ A {}`
//! reads D5, and `class F extends A /** D6 */ {}` D6); a function or a
//! method takes before its name, since `returns_ref` needs the lookahead
//! and `function_name` does not (`function /** X */ f() {}` reads X,
//! `function f /** X2 */ () {}` `false`); a trait before its name, an
//! enum case before `case` (`case /** C2 */ B = 1; case C = 2;` gives C2
//! to C), a parameter before its variable, a property accessor before
//! its name, a closure before its `(`; a property or constant declarator after
//! its last token, since a `=` or an operator may follow (`/** P */ public
//! $x, $y;` gives P to `$x` alone, `public $z /** P3 */;` P3 to `$z`, and
//! `const E = 1 /** K3 */, F = 2;` K3 to E). An attribute group takes at
//! its `#[` and [`Event::DocCommentRestore`] puts what it took back after
//! its `]`, php's `CG(doc_comment) = $2` in the `attribute` rule, so
//! `/** D4 */ #[Attr(static function(){})] class Y {}` reads D4 past the
//! closure's `}` and `#[Attr(/** IN */ 1)] class Y {}` reads `false`.
//! Every position was probed under the pin; the tests of
//! `tests/doc_comment.rs` hold each. A node whose kind does not opt into
//! `HasDocComment` still takes, which keeps the state right, and its
//! `resolve` reads nothing of it.
//!
//! **The line.** Every node records php's `zend_ast.lineno` at `Finish`,
//! from its children and the tokens around it, by the rule in [`line`];
//! the builder keeps what that rule reads: the last significant token
//! consumed before the node opened, the last consumed when it closes and
//! the next one after it.
//!
//! **A required child that is absent.** `!` is the parser's promise
//! (convention A2), and the typed struct cannot be built without the
//! field. The builder does not build it: the node stands in the shape as
//! `ERROR`, with its children, and [`Built::missing`] names the field, so
//! the fixture harness prints the broken promise beside the dump and no
//! parent reads a node that was never resolved.

use bumpalo::collections::Vec as BumpVec;

use crate::SyntaxKind;
use crate::arena::Arena;
use crate::lexer::Token;
use crate::line_index::LineIndex;
use crate::source::TextRange;

use super::{Building, Census, Element, Pools, Resolved, TokenId, Tree, line, resolve};

/// One parser event. The parser emits a flat stream; the builder assembles
/// the tree, so the parser is tested without trees and the builder without
/// a grammar. A diagnostic the parser raises is the parser's, beside the
/// stream ([`crate::parser::Parser::finish`]), so an event is plain
/// data and the stream is given up with nothing to drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Opens a node. `forward_parent` chains to a later `Start` that
    /// becomes this node's parent: `Marker::precede`, which the Pratt core
    /// uses to wrap a left operand after the fact.
    Start {
        /// The node's kind; [`TOMBSTONE`] for an abandoned marker.
        kind: SyntaxKind,
        /// The distance forward, in events, to the chained parent's `Start`.
        forward_parent: Option<u32>,
    },
    /// Closes the most recently opened node.
    Finish,
    /// Consumes the next significant token; `remap` is the kind the tree
    /// gives it (a keyword consumed as a name becomes `IDENT`).
    Token {
        /// The kind the tree reads.
        remap: SyntaxKind,
    },
    /// php's `backup_doc_comment` at this position of the grammar: the
    /// innermost open node takes the pending doc comment, and none is
    /// pending after (the docs above).
    DocComment,
    /// The `attribute` rule's `CG(doc_comment) = $2`: what the innermost
    /// open node took at its `DocComment` is pending again.
    DocCommentRestore,
}

/// How many nodes the builder makes room for open at once, and for the
/// children pending under them, before it grows: what a small source
/// nests, and under the size the allocator serves fastest.
const OPEN: usize = 4;

/// The kind of an abandoned `Start`, which the builder skips. `EOF` is
/// never a node, so it is free to mean this.
pub const TOMBSTONE: SyntaxKind = SyntaxKind::EOF;

/// A required child (convention A2) the parser did not supply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Missing {
    /// The field, as `Kind.field`: `ExprStmt.expr`.
    pub field: &'static str,
    /// Where the node that lacks it stands.
    pub range: TextRange,
}

/// What one build produced.
#[derive(Debug, Default)]
pub struct Built<'a> {
    /// The tree, in the arena the build was handed.
    pub tree: Tree<'a>,
    /// Every required child the parser did not supply, in finish order.
    pub missing: Vec<Missing>,
}

/// One open node.
struct Frame {
    kind: SyntaxKind,
    /// The node's placeholder in the shape.
    at: usize,
    /// Where the node's direct children begin in `pending`.
    children_from: usize,
    /// The doc comment the node took at its `DocComment` event.
    doc_comment: Option<TokenId>,
    /// The last significant token consumed before the node opened, which
    /// is what php's scanner had last returned when the node's first rule
    /// began ([`line`]).
    before: Option<usize>,
    /// The kind of the innermost frame below this one that opened after a
    /// different token, `Site::enclosing` in [`line`]. The frames below an
    /// open one do not change while it is open, so this is read once, at
    /// `Start`, off the frame directly below: its own kind where it
    /// opened elsewhere, its own `enclosing` where it opened at the same
    /// token. A left-nested chain opens every link at one token, and
    /// looking down the open frames at each `Finish` instead cost the
    /// chain's length per link.
    enclosing: Option<SyntaxKind>,
}

struct Builder<'s, 'a> {
    tokens: &'s [Token],
    lines: &'s LineIndex<'s>,
    /// The next token not yet passed over.
    raw: usize,
    /// The last significant token consumed.
    prev: Option<usize>,
    /// php's `CG(doc_comment)`: the pending doc comment (the docs above).
    doc_comment: Option<TokenId>,
    elements: BumpVec<'a, Element>,
    /// The direct children of every open frame, innermost last.
    pending: Vec<Element>,
    frames: Vec<Frame>,
    /// The kinds of one forward-parent chain while it is opened, kept
    /// between chains so a chain costs no allocation of its own.
    chain: Vec<SyntaxKind>,
    nodes: Building<'a>,
    pools: &'s mut Pools,
    missing: Vec<Missing>,
}

/// Build the tree of `events` over `tokens`, the lossless stream the
/// parser read, into `arena`, with `lines` the line table of the source
/// the tokens index, which every node's line is read from. Unbalanced
/// events do not fail the build: a `Finish` with nothing open is
/// dropped, and a frame still open at the end is finished.
#[must_use]
pub fn build<'a>(
    arena: &'a Arena,
    tokens: &[Token],
    lines: &LineIndex<'_>,
    events: Vec<Event>,
) -> Built<'a> {
    let census = Census::of(&events, Vec::new());
    build_in(arena, tokens, lines, events, census, &mut Stacks::default())
}

/// The builder's stacks that die with the build: the open frames, their
/// pending children, a forward-parent chain, the event stream once
/// read, the room of the [`Census`] the parser counts each node kind's
/// nodes into, and the lists' pools before they are copied into the
/// arena. A parse lends them ([`crate::parse`]) and takes them back
/// empty, so a thread that parses again reuses their room and a small
/// parse allocates none of them, and lays down only the counts and the
/// pools its tree has.
#[derive(Default)]
pub(crate) struct Stacks {
    pub(crate) events: Vec<Event>,
    pending: Vec<Element>,
    frames: Vec<Frame>,
    chain: Vec<SyntaxKind>,
    counts: Vec<u32>,
    pools: Pools,
}

impl Stacks {
    /// A census at zero over the counts' room, for the parser to count
    /// into.
    pub(crate) fn census(&mut self) -> Census {
        Census::new(std::mem::take(&mut self.counts))
    }

    /// Whether every stack's room is within `bytes`, the most a thread
    /// keeps between parses.
    pub(crate) fn within(&self, bytes: usize) -> bool {
        self.events.capacity() * size_of::<Event>() <= bytes
            && self.pending.capacity() * size_of::<Element>() <= bytes
            && self.frames.capacity() * size_of::<Frame>() <= bytes
            && self.chain.capacity() * size_of::<SyntaxKind>() <= bytes
            && self.pools.within(bytes)
    }
}

/// One of [`Stacks`] taken for a build, with room for `at_least`.
fn lent<T>(stack: &mut Vec<T>, at_least: usize) -> Vec<T> {
    let mut stack = std::mem::take(stack);
    stack.reserve(at_least);
    stack
}

/// [`build`] of `events`, which `census` counted, over the stacks
/// `stacks` lends, handed back empty with `events`' room among them.
pub(crate) fn build_in<'a>(
    arena: &'a Arena,
    tokens: &[Token],
    lines: &LineIndex<'_>,
    mut events: Vec<Event>,
    census: Census,
    stacks: &mut Stacks,
) -> Built<'a> {
    // The shape holds an element per `Start` and per `Token`, so it is
    // laid down once at that length, and grows only where the root takes
    // tokens the parser left unconsumed; each kind's nodes are laid down
    // at the count of its `Start`s.
    let bump = arena.bump();
    let shaped = census.shaped();
    let nodes = Building::new(bump, census);
    let mut builder = Builder {
        tokens,
        lines,
        raw: 0,
        prev: None,
        doc_comment: None,
        elements: BumpVec::with_capacity_in(shaped, bump),
        pending: lent(&mut stacks.pending, shaped.min(2 * OPEN)),
        frames: lent(&mut stacks.frames, OPEN),
        chain: std::mem::take(&mut stacks.chain),
        nodes,
        pools: &mut stacks.pools,
        missing: Vec::new(),
    };
    for i in 0..events.len() {
        // A chain only reaches forward and tombstones the `Start`s it
        // opened, so an event once passed is never read again.
        let Some(&event) = events.get(i) else {
            break;
        };
        match event {
            Event::Start { kind, .. } if kind == TOMBSTONE => {}
            Event::Start {
                kind,
                forward_parent: None,
            } => builder.start(kind),
            Event::Start {
                kind,
                forward_parent: Some(delta),
            } => builder.start_chain(&mut events, i, kind, delta),
            Event::Finish => builder.finish(),
            Event::Token { remap } => builder.token(remap),
            Event::DocComment => builder.take_doc_comment(),
            Event::DocCommentRestore => builder.restore_doc_comment(),
        }
    }
    while !builder.frames.is_empty() {
        builder.finish();
    }
    let Builder {
        elements,
        mut pending,
        frames,
        mut chain,
        nodes,
        pools,
        missing,
        ..
    } = builder;
    let lists = bump.alloc_with(|| pools.freeze(bump));
    let (nodes, counts) = nodes.freeze();
    events.clear();
    pending.clear();
    chain.clear();
    stacks.events = events;
    stacks.pending = pending;
    stacks.frames = frames;
    stacks.chain = chain;
    stacks.counts = counts;
    Built {
        tree: Tree {
            elements: elements.into_bump_slice(),
            nodes,
            lists,
        },
        missing,
    }
}

impl Builder<'_, '_> {
    /// Open a forward-parent chain from the `Start` at `at` in `events`,
    /// of `kind`, whose parent's `Start` stands `delta` further on: the
    /// innermost first, so the chain is opened in reverse, and each later
    /// `Start` is tombstoned so it is not opened twice.
    fn start_chain(&mut self, events: &mut [Event], mut at: usize, kind: SyntaxKind, delta: u32) {
        let mut chain = std::mem::take(&mut self.chain);
        chain.push(kind);
        let mut next = Some(delta);
        while let Some(delta) = next {
            at += delta as usize;
            match events.get_mut(at) {
                Some(Event::Start {
                    kind,
                    forward_parent,
                }) if *kind != TOMBSTONE => {
                    chain.push(*kind);
                    next = *forward_parent;
                    *kind = TOMBSTONE;
                }
                // A link into nothing, or into a tombstone, ends the
                // chain; the parser does not emit either.
                _ => next = None,
            }
        }
        for &kind in chain.iter().rev() {
            self.start(kind);
        }
        chain.clear();
        self.chain = chain;
    }

    /// Pass over the whitespace and comments before the next significant
    /// token, tracking php's pending doc comment.
    fn skip_insignificant(&mut self) {
        while let Some(token) = self.tokens.get(self.raw)
            && token.kind.is_trivia()
        {
            if token.kind == SyntaxKind::DOC_COMMENT {
                self.doc_comment = Some(TokenId(index(self.raw)));
            }
            self.raw += 1;
        }
    }

    /// The offset an empty node stands at: the next significant token's,
    /// or the end of the source.
    fn position(&mut self) -> u32 {
        self.skip_insignificant();
        match self.tokens.get(self.raw) {
            Some(token) => token.offset,
            None => self.tokens.last().map_or(0, |t| t.offset + t.len),
        }
    }

    /// Open one node; a chain opens its nodes outermost first, each at
    /// the same token.
    fn start(&mut self, kind: SyntaxKind) {
        self.skip_insignificant();
        let before = self.prev;
        let enclosing = self.frames.last().and_then(|below| {
            if below.before == before {
                below.enclosing
            } else {
                Some(below.kind)
            }
        });
        self.frames.push(Frame {
            kind,
            at: self.elements.len(),
            children_from: self.pending.len(),
            doc_comment: None,
            before,
            enclosing,
        });
        self.elements.push(Element::Node {
            kind,
            range: TextRange::new(0, 0),
            line: 0,
            index: 0,
            descendants: 0,
        });
    }

    /// php's `backup_doc_comment`: the innermost open node takes the
    /// pending doc comment, with the whitespace and comments before the
    /// next significant token passed over first, since php's scanner had
    /// passed them when the grammar placed the event (the docs above).
    fn take_doc_comment(&mut self) {
        self.skip_insignificant();
        let taken = self.doc_comment.take();
        if let Some(frame) = self.frames.last_mut() {
            frame.doc_comment = taken;
        }
    }

    /// The `attribute` rule's restore: what the innermost open node took
    /// is pending again.
    fn restore_doc_comment(&mut self) {
        if let Some(frame) = self.frames.last() {
            self.doc_comment = frame.doc_comment;
        }
    }

    /// Consume the next significant token into the innermost open node.
    fn token(&mut self, remap: SyntaxKind) {
        self.skip_insignificant();
        let Some(token) = self.tokens.get(self.raw) else {
            // The parser consumed past the end; nothing to write.
            return;
        };
        if token.kind == SyntaxKind::R_CURLY {
            self.doc_comment = None;
        }
        let element = Element::Token {
            kind: remap,
            id: TokenId(index(self.raw)),
        };
        self.prev = Some(self.raw);
        self.raw += 1;
        self.elements.push(element);
        self.pending.push(element);
    }

    /// Close the innermost open node: resolve it, place it in the shape,
    /// and hand it to its parent as a child.
    fn finish(&mut self) {
        let Some(frame) = self.frames.pop() else {
            return;
        };
        if self.frames.is_empty() {
            // The root: what the parser left unconsumed is its last
            // children, by the lexer's kinds.
            loop {
                self.skip_insignificant();
                let Some(token) = self.tokens.get(self.raw) else {
                    break;
                };
                self.token(token.kind);
            }
        }
        let children = &self.pending[frame.children_from..];
        let ends = children.first().copied().zip(children.last().copied());
        let range = if let Some((first, last)) = ends {
            TextRange::new(self.start_of(first), self.end_of(last))
        } else {
            let at = self.position();
            TextRange::new(at, at)
        };
        let site = line::Site {
            tokens: self.tokens,
            lines: self.lines,
            before: frame.before,
            last: self.prev,
            raw: self.raw,
            enclosing: frame.enclosing,
        };
        let children = &self.pending[frame.children_from..];
        let line = line::of(frame.kind, children, &site);
        let (kind, index) = match resolve(
            &mut self.nodes,
            self.pools,
            frame.kind,
            range,
            line,
            children,
            frame.doc_comment,
        ) {
            Resolved::Node(index) => (frame.kind, index),
            Resolved::Missing(field) => {
                self.missing.push(Missing { field, range });
                (SyntaxKind::ERROR, 0)
            }
            Resolved::Untyped => (frame.kind, 0),
        };
        let element = Element::Node {
            kind,
            range,
            line,
            index,
            descendants: index_from(self.elements.len() - frame.at - 1),
        };
        self.elements[frame.at] = element;
        self.pending.truncate(frame.children_from);
        self.pending.push(element);
    }

    fn start_of(&self, element: Element) -> u32 {
        match element {
            Element::Node { range, .. } => range.start,
            Element::Token { id, .. } => self.tokens.get(id.0 as usize).map_or(0, |t| t.offset),
        }
    }

    fn end_of(&self, element: Element) -> u32 {
        match element {
            Element::Node { range, .. } => range.end(),
            Element::Token { id, .. } => self
                .tokens
                .get(id.0 as usize)
                .map_or(0, |t| t.offset + t.len),
        }
    }
}

/// A position in the token stream as a `TokenId` carries it. The stream
/// is over a source of at most `u32::MAX` bytes and every token has at
/// least one, so the conversion holds; it saturates rather than fails.
fn index(raw: usize) -> u32 {
    u32::try_from(raw).unwrap_or(u32::MAX)
}

/// A count of elements as the shape carries it; saturating, as [`index`].
fn index_from(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::LexOptions;
    use crate::source::FileId;
    use crate::tree::{ClassDecl, FunctionDecl, HasDocComment, PropertyDeclarator, dump};

    fn start(kind: SyntaxKind) -> Event {
        Event::Start {
            kind,
            forward_parent: None,
        }
    }

    fn token(kind: SyntaxKind) -> Event {
        Event::Token { remap: kind }
    }

    fn lexed(source: &[u8]) -> Vec<Token> {
        crate::lexer::lex(FileId(0), source, &LexOptions::default()).tokens
    }

    /// The build of `events` in an arena of its own, kept for the test.
    fn build_over(source: &[u8], tokens: &[Token], events: Vec<Event>) -> Built<'static> {
        let arena: &'static Arena = Box::leak(Box::new(Arena::new()));
        build(arena, tokens, &LineIndex::new(arena, source), events)
    }

    /// `class X {}` as events, under an open `SOURCE_FILE`; the take
    /// stands before the `{`, as the grammar places it.
    fn class_decl() -> Vec<Event> {
        use SyntaxKind::*;
        vec![
            start(CLASS_DECL),
            token(CLASS_KW),
            token(IDENT),
            Event::DocComment,
            start(CLASS_BODY),
            token(L_CURLY),
            token(R_CURLY),
            Event::Finish,
            Event::Finish,
        ]
    }

    fn file(body: impl IntoIterator<Item = Event>) -> Vec<Event> {
        let mut events = vec![start(SyntaxKind::SOURCE_FILE), token(SyntaxKind::OPEN_TAG)];
        events.extend(body);
        events.push(Event::Finish);
        events
    }

    fn text<'a>(source: &'a [u8], tokens: &[Token], id: Option<TokenId>) -> Option<&'a [u8]> {
        let token = tokens[id?.0 as usize];
        Some(&source[token.offset as usize..(token.offset + token.len) as usize])
    }

    /// php's rule, measured under the pin at 8.5.9 with
    /// `ReflectionClass::getDocComment()` (the docs above): a blank line
    /// does not detach the comment (D1), a statement between does not
    /// either (D2), the `}` of a function body resets it (D3 is after
    /// that `}`), and a `}` with no comment after leaves none (D7).
    #[test]
    fn the_doc_comment_is_the_last_one_not_followed_by_a_close_brace() {
        use SyntaxKind::*;
        let source = b"<?php\n/** D1 */\n\nclass A {}\n/** D2 */ $x = 1; class B {}\nfunction f() {} /** D3 */\nclass C {}\n/** D7 */ if (1) { } class G {}";
        let tokens = lexed(source);
        let built = build_over(
            source,
            &tokens,
            file(
                [
                    class_decl(),
                    vec![
                        start(EXPR_STMT),
                        start(ASSIGN_EXPR),
                        start(SIMPLE_VAR_EXPR),
                        token(VARIABLE),
                        Event::Finish,
                        token(EQ),
                        start(LITERAL_EXPR),
                        token(INT),
                        Event::Finish,
                        Event::Finish,
                        token(SEMICOLON),
                        Event::Finish,
                    ],
                    class_decl(),
                    vec![
                        start(FUNCTION_DECL),
                        token(FUNCTION_KW),
                        Event::DocComment,
                        token(IDENT),
                        start(PARAM_LIST),
                        token(L_PAREN),
                        token(R_PAREN),
                        Event::Finish,
                        start(BLOCK),
                        token(L_CURLY),
                        token(R_CURLY),
                        Event::Finish,
                        Event::Finish,
                    ],
                    class_decl(),
                    vec![
                        start(IF_STMT),
                        token(IF_KW),
                        token(L_PAREN),
                        start(LITERAL_EXPR),
                        token(INT),
                        Event::Finish,
                        token(R_PAREN),
                        start(BLOCK),
                        token(L_CURLY),
                        token(R_CURLY),
                        Event::Finish,
                        Event::Finish,
                    ],
                    class_decl(),
                ]
                .into_iter()
                .flatten(),
            ),
        );
        assert!(
            built.missing.is_empty(),
            "{}",
            dump(&built.tree, &tokens, source)
        );
        let docs: Vec<Option<&[u8]>> = built
            .tree
            .nodes::<ClassDecl>()
            .iter()
            .map(|class| text(source, &tokens, class.doc_comment()))
            .collect();
        assert_eq!(
            docs,
            [
                Some(&b"/** D1 */"[..]),
                Some(b"/** D2 */"),
                Some(b"/** D3 */"),
                None
            ]
        );
        let f = &built.tree.nodes::<FunctionDecl>()[0];
        assert_eq!(f.doc_comment(), None, "D2 was taken by class B");
    }

    /// Taken, then cleared: one doc comment reaches one declarator, and a
    /// member's doc comment is its own (the class body's `{` resets
    /// nothing; php resets on `}` alone).
    #[test]
    fn a_doc_comment_is_taken_once() {
        use SyntaxKind::*;
        let source = b"<?php /** C */ class A { /** P */ public $x, $y; }";
        let tokens = lexed(source);
        let declarator = |events: &mut Vec<Event>| {
            events.extend([
                start(PROPERTY_DECLARATOR),
                token(VARIABLE),
                Event::DocComment,
                Event::Finish,
            ]);
        };
        let mut events = vec![
            start(CLASS_DECL),
            token(CLASS_KW),
            token(IDENT),
            Event::DocComment,
            start(CLASS_BODY),
            token(L_CURLY),
            start(PROPERTY_DECL),
            token(PUBLIC_KW),
        ];
        declarator(&mut events);
        events.push(token(COMMA));
        declarator(&mut events);
        events.extend([token(SEMICOLON), Event::Finish]);
        events.extend([token(R_CURLY), Event::Finish, Event::Finish]);
        let built = build_over(source, &tokens, file(events));
        assert!(
            built.missing.is_empty(),
            "{}",
            dump(&built.tree, &tokens, source)
        );
        let class = &built.tree.nodes::<ClassDecl>()[0];
        assert_eq!(
            text(source, &tokens, class.doc_comment()),
            Some(&b"/** C */"[..])
        );
        let docs: Vec<Option<&[u8]>> = built
            .tree
            .nodes::<PropertyDeclarator>()
            .iter()
            .map(|p| text(source, &tokens, p.doc_comment()))
            .collect();
        assert_eq!(docs, [Some(&b"/** P */"[..]), None]);
    }

    /// The take passes over the whitespace and comments before the next
    /// significant token first, so a comment between the class name and
    /// `{` is the class's,
    /// and a take where no node is open drops the comment, as php's
    /// `backup_doc_comment` returns it to a rule that keeps nothing.
    #[test]
    fn a_take_passes_over_the_comments_before_it() {
        let source = b"<?php class A /** D6 */ {}";
        let tokens = lexed(source);
        let built = build_over(source, &tokens, file(class_decl()));
        let class = &built.tree.nodes::<ClassDecl>()[0];
        assert_eq!(
            text(source, &tokens, class.doc_comment()),
            Some(&b"/** D6 */"[..])
        );
        let built = build_over(
            source,
            &tokens,
            vec![Event::DocComment, Event::DocCommentRestore],
        );
        assert!(built.tree.nodes::<ClassDecl>().is_empty());
    }

    /// The attribute's restore: what the group took at `#[` is pending
    /// again after `]`, past a `}` inside the group.
    #[test]
    fn an_attribute_group_gives_back_what_it_took() {
        use SyntaxKind::*;
        let source = b"<?php /** D4 */ #[A(function(){})] class Y {}";
        let tokens = lexed(source);
        let events = vec![
            start(CLASS_DECL),
            start(ATTRIBUTE_GROUP),
            Event::DocComment,
            token(ATTRIBUTE_OPEN),
            start(ATTRIBUTE),
            token(IDENT),
            start(ARG_LIST),
            token(L_PAREN),
            start(CLOSURE_EXPR),
            token(FUNCTION_KW),
            Event::DocComment,
            start(PARAM_LIST),
            token(L_PAREN),
            token(R_PAREN),
            Event::Finish,
            start(BLOCK),
            token(L_CURLY),
            token(R_CURLY),
            Event::Finish,
            Event::Finish,
            token(R_PAREN),
            Event::Finish,
            Event::Finish,
            token(R_BRACK),
            Event::DocCommentRestore,
            Event::Finish,
            token(CLASS_KW),
            token(IDENT),
            Event::DocComment,
            start(CLASS_BODY),
            token(L_CURLY),
            token(R_CURLY),
            Event::Finish,
            Event::Finish,
        ];
        let built = build_over(source, &tokens, file(events));
        assert!(
            built.missing.is_empty(),
            "{}",
            dump(&built.tree, &tokens, source)
        );
        let class = &built.tree.nodes::<ClassDecl>()[0];
        assert_eq!(
            text(source, &tokens, class.doc_comment()),
            Some(&b"/** D4 */"[..])
        );
    }

    /// What the parser did not consume is the root's last children, an
    /// empty node stands at the next significant token, and events out of
    /// balance neither drop a node nor fail the build.
    #[test]
    fn unconsumed_tokens_empty_nodes_and_unbalanced_events() {
        use SyntaxKind::*;
        let source = b"<?php ; $x";
        let tokens = lexed(source);
        let built = build_over(
            source,
            &tokens,
            vec![
                start(SOURCE_FILE),
                token(OPEN_TAG),
                start(EMPTY_STMT),
                token(SEMICOLON),
                Event::Finish,
                start(ERROR),
                Event::Finish,
                Event::Finish,
                Event::Finish,
                start(EXPR_STMT),
                token(VARIABLE),
            ],
        );
        assert_eq!(
            dump(&built.tree, &tokens, source),
            "SOURCE_FILE@0..10\n\
             \x20 OPEN_TAG@0..6 \"<?php \"\n\
             \x20 EMPTY_STMT@6..7\n\
             \x20   SEMICOLON@6..7 \";\"\n\
             \x20 ERROR@8..8\n\
             \x20 VARIABLE@8..10 \"$x\"\n\
             ERROR@10..10\n"
        );
        assert_eq!(built.missing.len(), 1, "{:?}", built.missing);
    }
}
