//! The tree (design §4.9, ADR-0003): one struct per node kind of
//! `php.ungram`, its children named fields the builder resolves once from
//! the parser's event stream, so a read is a field read and nothing scans.
//! A child that is a struct node is held as its [`NodeId`], a child that is
//! an alternation as the alternation's enum, whose variant says which node
//! or token stands there, and a token as its [`TokenId`], an index into the
//! lossless token stream. A child the schema marks `!` is a plain field;
//! every other child is `Option`, since under error recovery any child can
//! be absent (convention A2), and a repetition is a boxed slice. No node
//! knows its parent: the compiler passes scope downward.
//!
//! **How a node is addressed.** [`Tree`] owns [`Nodes`], the arena: one
//! vector per node kind, so a `NodeId<ClassDecl>` is a position among the
//! class declarations and `tree[id]` reads the kind's slot, then
//! indexes that kind's vector. A kind's vector is made with its first
//! node, so a tree pays for the kinds it holds and not for every kind
//! the schema names, in its making, its moves and its drop. Beside
//! the arena the tree keeps its shape, every node and every significant
//! token as one [`Element`] in source order with the size of each node's
//! subtree, which is what a walk that has no field to follow reads, the
//! dump first among them; an `ERROR` node, which no schema rule names,
//! lives in the shape alone.
//!
//! **What every node carries.** Its kind, its range and its line: `range`
//! is where the node is in the source, and `line` is php's
//! `zend_ast.lineno`, the one fact of the tree that is not a position in
//! the token stream, computed by the builder at `Finish` as php's parser
//! computes it ([`line`]) and read by the compiler where php's reads
//! `ast->lineno` (design §4.9, ledger 27).
//!
//! The nodes, the alternation enums, the mixin traits, the token sets, the
//! arena and the one-pass resolution are emitted by `build.rs` into
//! `OUT_DIR/tree.rs` and included below; what is written here by hand is
//! what every emitted item leans on. [`builder`] turns the event stream
//! into a tree and [`dump`] prints one.

mod builder;
mod dump;
mod line;

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ops::Index;

use crate::SyntaxKind;
use crate::source::TextRange;

pub use builder::{Built, Event, Missing, TOMBSTONE, build};
pub use dump::dump;

/// The index of one node in the tree's arena, typed by the node it
/// addresses: a `NodeId<ClassDecl>` answers a `ClassDecl` and nothing
/// else, so a field's type says what the child is and the read needs no
/// cast. Four bytes; `Copy`, `Eq` and `Hash` whatever the node, since the
/// node is a phantom. An id is a position in the tree that built it and
/// reads nothing in another.
pub struct NodeId<T> {
    index: u32,
    node: PhantomData<fn() -> T>,
}

impl<T> NodeId<T> {
    /// The id of the node at `index` in the arena.
    #[must_use]
    pub fn new(index: u32) -> Self {
        NodeId {
            index,
            node: PhantomData,
        }
    }

    /// The position in the arena, among the nodes of this kind.
    #[must_use]
    pub fn index(self) -> u32 {
        self.index
    }
}

impl<T> Clone for NodeId<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for NodeId<T> {}

impl<T> PartialEq for NodeId<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl<T> Eq for NodeId<T> {}

impl<T> Hash for NodeId<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state);
    }
}

impl<T> fmt::Debug for NodeId<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.index)
    }
}

/// The index of one token in the token stream a tree was built over. A
/// node references significant tokens only; the stream keeps every token,
/// so the index counts whitespace and comments too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TokenId(pub u32);

/// What every emitted node is: a kind, a range in the source, php's line,
/// and a place in the arena.
pub trait Node: Sized {
    /// The node's kind, the one `SyntaxKind` its struct maps to (convention
    /// A1).
    const KIND: SyntaxKind;

    /// Where the node is in the source: from its first significant token
    /// to its last.
    fn range(&self) -> TextRange;

    /// php's `zend_ast.lineno` for this node, as the builder recorded it
    /// ([`line`]): the line of its first child in php's child order, and
    /// the scanner's line at the reduction for a node php builds without
    /// one. What `zend_compile.c` sets `CG(zend_lineno)` to before it
    /// compiles the node, so what an opline carries.
    fn line(&self) -> u32;

    /// Every node of this kind in the arena, in the order the builder
    /// finished them; a `NodeId<Self>` indexes this slice.
    fn all(nodes: &Nodes) -> &[Self];
}

/// One element of the tree's shape: a node, or a significant token by the
/// kind the tree gives it, which for a keyword consumed as a name is
/// `IDENT` where the token stream keeps the keyword.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Element {
    /// A node: its kind, its range, its position among the nodes of its
    /// kind in the arena (none for an `ERROR` node, which has no struct,
    /// and `0` is written), and how many elements its subtree holds after
    /// it.
    Node {
        /// The node's kind.
        kind: SyntaxKind,
        /// From the first significant token to the last; empty, at the
        /// next token's offset, for a node with none.
        range: TextRange,
        /// php's `zend_ast.lineno`, what [`Node::line`] answers for the
        /// node's struct, here so a parent's line is read from its child
        /// as the builder closes it, and so an `ERROR` node has one.
        line: u32,
        /// The index a `NodeId` of this kind carries.
        index: u32,
        /// The elements inside the node, which follow it in the shape.
        descendants: u32,
    },
    /// A significant token, by the kind the tree gives it and its place in
    /// the token stream.
    Token {
        /// The kind as the tree reads it: the parser's remap where it made
        /// one, the lexer's kind otherwise.
        kind: SyntaxKind,
        /// The token in the stream.
        id: TokenId,
    },
}

impl Element {
    /// The element's kind.
    #[must_use]
    pub fn kind(self) -> SyntaxKind {
        match self {
            Element::Node { kind, .. } | Element::Token { kind, .. } => kind,
        }
    }

    /// The node this element is, as the id its kind's struct answers to,
    /// or `None` for a token or a node of another kind.
    #[must_use]
    pub fn id<T: Node>(self) -> Option<NodeId<T>> {
        match self {
            Element::Node { kind, index, .. } if kind == T::KIND => Some(NodeId::new(index)),
            _ => None,
        }
    }
}

/// The tree of one source file: the arena and the shape, as the docs
/// above say. An empty tree has no root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    /// The shape: every node and significant token in source order, a
    /// node before its subtree.
    elements: Box<[Element]>,
    /// The arena.
    nodes: Nodes,
}

impl Tree {
    /// The shape, in source order: a node, then the elements inside it.
    #[must_use]
    pub fn elements(&self) -> &[Element] {
        &self.elements
    }

    /// The outermost node, or `None` for an empty tree.
    #[must_use]
    pub fn root(&self) -> Option<Element> {
        self.elements.first().copied()
    }

    /// Every node of one kind, in the order the builder finished them:
    /// children before their parent, so the root is last.
    #[must_use]
    pub fn nodes<T: Node>(&self) -> &[T] {
        T::all(&self.nodes)
    }
}

impl<T: Node> Index<NodeId<T>> for Tree {
    type Output = T;

    fn index(&self, id: NodeId<T>) -> &T {
        &T::all(&self.nodes)[id.index as usize]
    }
}

/// What resolving one finished node came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// The node's struct is in the arena at this index.
    Node(u32),
    /// A required child (`!`) is absent, named `Kind.field`; no struct was
    /// built. `Nodes.kind` names the other failure that leaves a node
    /// unbuilt, the arena having no vector to put the kind in, which the
    /// codegen's count of slots rules out; it is reported where the
    /// grammar's broken promises are, never put in another kind's place.
    Missing(&'static str),
    /// The kind has no struct: `ERROR`, or a token kind opened as a node.
    Untyped,
}

impl Nodes {
    /// The vector of the struct kind at `slot`, none while no node of
    /// the kind is built.
    fn store(&self, slot: usize) -> Option<&Store> {
        let at = (*self.slots.get(slot)?)?;
        self.stores.get(usize::from(at))
    }

    /// The vector of the struct kind at `slot`, made empty by `empty`
    /// with the kind's first node. `None` only where the slot is not a
    /// kind's, or more kinds stand than a slot counts; the caller's
    /// match on the kind's variant is what reads the vector.
    fn open<T>(&mut self, slot: usize, empty: fn(Vec<T>) -> Store) -> Option<&mut Store> {
        let at = if let Some(at) = *self.slots.get(slot)? {
            at
        } else {
            let at = u8::try_from(self.stores.len()).ok()?;
            self.stores.push(empty(Vec::new()));
            *self.slots.get_mut(slot)? = Some(at);
            at
        };
        self.stores.get_mut(usize::from(at))
    }
}

/// One node into its kind's vector, and where it went.
fn push<T>(nodes: &mut Vec<T>, node: Result<T, &'static str>) -> Resolved {
    match node {
        Ok(node) => {
            // A file the lexer accepts is under `u32::MAX` bytes and a
            // node without a token of its own is bounded by the parser's
            // forward-progress fuel per token, so the count fits; the
            // conversion saturates rather than fails.
            let index = u32::try_from(nodes.len()).unwrap_or(u32::MAX);
            nodes.push(node);
            Resolved::Node(index)
        }
        Err(field) => Resolved::Missing(field),
    }
}

/// What a child reads as when the builder fills a field of its parent:
/// the node's id where the element is a node of the kind, the
/// alternation's variant where the element is one of its listed nodes or
/// tokens.
pub(crate) trait Child: Sized {
    /// This child, if `element` is one.
    fn of(element: &Element) -> Option<Self>;
}

impl<T: Node> Child for NodeId<T> {
    fn of(element: &Element) -> Option<Self> {
        element.id::<T>()
    }
}

/// The first child the field admits.
fn first<T: Child>(children: &[Element]) -> Option<T> {
    children.iter().find_map(T::of)
}

/// The first child the field admits after a token in `anchors`, and none
/// once a token in `stops` is passed (A4: `TernaryExpr.then` is the
/// expression between `?` and `:`). An anchor token is never the child.
fn first_after<T: Child>(
    children: &[Element],
    anchors: &[SyntaxKind],
    stops: &[SyntaxKind],
) -> Option<T> {
    let mut armed = false;
    for element in children {
        if let Element::Token { kind, .. } = element {
            if armed && stops.contains(kind) {
                return None;
            }
            if anchors.contains(kind) {
                armed = true;
                continue;
            }
        }
        if armed && let Some(child) = T::of(element) {
            return Some(child);
        }
    }
    None
}

/// Every child the field admits, in order. Counted first, so the slice
/// is one allocation of its own length and none where the list is empty.
fn all<T: Child>(children: &[Element]) -> Box<[T]> {
    exact(children.iter().filter_map(T::of))
}

/// What `items` yields, in one allocation of exactly its length: walked
/// once to count and once to fill, where collecting grew a vector and
/// then reallocated it down to a slice.
fn exact<T>(items: impl Iterator<Item = T> + Clone) -> Box<[T]> {
    let count = items.clone().count();
    if count == 0 {
        return Box::default();
    }
    let mut out = Vec::with_capacity(count);
    out.extend(items);
    out.into_boxed_slice()
}

/// The first token whose kind is in `kinds`, after a token in `anchors`
/// where there are any.
fn token(children: &[Element], anchors: &[SyntaxKind], kinds: &[SyntaxKind]) -> Option<TokenId> {
    let mut armed = anchors.is_empty();
    for element in children {
        if let Element::Token { kind, id } = element {
            if anchors.contains(kind) {
                armed = true;
            } else if armed && kinds.contains(kind) {
                return Some(*id);
            }
        }
    }
    None
}

/// Every token whose kind is in `kinds`, in order.
fn tokens(children: &[Element], kinds: &[SyntaxKind]) -> Box<[TokenId]> {
    exact(children.iter().filter_map(|element| match element {
        Element::Token { kind, id } if kinds.contains(kind) => Some(*id),
        _ => None,
    }))
}

/// Is this token a member name: an `IDENT`, or, since the parser remaps a
/// keyword consumed as a name to `IDENT` at build time, any keyword kind
/// should a remap be missed.
fn is_member(kind: SyntaxKind) -> bool {
    kind == SyntaxKind::IDENT || kind.is_keyword()
}

/// The first member-name token, after a token in `anchors` where there
/// are any.
fn member(children: &[Element], anchors: &[SyntaxKind]) -> Option<TokenId> {
    let mut armed = anchors.is_empty();
    for element in children {
        if let Element::Token { kind, id } = element {
            if anchors.contains(kind) {
                armed = true;
            } else if armed && is_member(*kind) {
                return Some(*id);
            }
        }
    }
    None
}

/// Where a token child stands in the source, for an alternation's `range`:
/// from its first byte to its last, and empty at the end of the source for
/// a token the stream lacks.
fn token_range(parsed: &crate::Parsed, id: TokenId) -> TextRange {
    match parsed.tokens.get(id.0 as usize) {
        Some(token) => TextRange::new(token.offset, token.offset.saturating_add(token.len)),
        None => TextRange::new(parsed.lines.len(), parsed.lines.len()),
    }
}

/// The line a token child starts on, for an alternation's `line`: what the
/// builder gives a token child of a node ([`line`]).
fn token_line(parsed: &crate::Parsed, id: TokenId) -> u32 {
    parsed.lines.line_number(token_range(parsed, id).start)
}

include!(concat!(env!("OUT_DIR"), "/tree.rs"));
