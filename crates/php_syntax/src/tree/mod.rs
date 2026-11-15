//! The tree (design §4.9, ADR-0003): one struct per node kind of
//! `php.ungram`, its children named fields the builder resolves once from
//! the parser's event stream, so a read is a field read and nothing scans.
//! A child that is a struct node is held as its [`NodeId`], a child that is
//! an alternation as the alternation's enum, whose variant says which node
//! or token stands there, and a token as its [`TokenId`], an index into the
//! lossless token stream. A child the schema marks `!` is a plain field;
//! every other child is `Option`, since under error recovery any child can
//! be absent (convention A2), and a repetition is a [`List`], a stretch of one
//! of the tree's pools. No node knows its parent: the compiler passes
//! scope downward.
//!
//! **Where it lives.** In the [`crate::Arena`] the parse was handed, and
//! nowhere else: a node is plain data, `Copy` and with nothing to
//! drop, so the tree is given up with the arena, whole, and a [`Tree`] is
//! three references into it.
//!
//! **How a node is addressed.** [`Tree`] reads [`Nodes`], the arena of
//! nodes: one slice per node kind, so a `NodeId<ClassDecl>` is a position
//! among the class declarations and `tree[id]` reads the kind's slot,
//! then indexes that kind's slice. A kind's slice is made with its first
//! node, at the count of the kind's nodes the event stream holds, so a
//! tree pays for the kinds it holds and not for every kind the schema
//! names, and no kind's slice grows. A list is addressed the same way:
//! [`Lists`] keeps one pool per element type, a node's `List<Stmt>` is
//! where its stretch of the statements' pool starts and how long it is, and
//! `tree[list]` is the slice. Beside the arena the tree keeps its shape,
//! every node and every significant token as one [`Element`] in source
//! order with the size of each node's subtree, which is what a walk that
//! has no field to follow reads, the dump first among them; an `ERROR`
//! node, which no schema rule names, lives in the shape alone.
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

use php_heap::{ArenaVec, MappedVec, ParseArena};

use crate::SyntaxKind;
use crate::source::TextRange;

pub use builder::{Built, Event, Missing, TOMBSTONE, build};
pub(crate) use builder::{Stacks, build_in};
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
    fn all<'t>(nodes: &Nodes<'t>) -> &'t [Self];
}

/// One repetition of a node (convention A2), a stretch of the pool its
/// element type has in the tree's [`Lists`]: where the stretch starts and how
/// long it is, typed by what it lists, so a `List<Stmt>` answers
/// statements and nothing else. Eight bytes and `Copy`; `tree[list]`
/// reads the slice. A list is a position in the tree that built it and
/// reads nothing in another.
pub struct List<T> {
    start: u32,
    len: u32,
    item: PhantomData<fn() -> T>,
}

impl<T> List<T> {
    /// The list of `len` items from `start` in its pool.
    #[must_use]
    pub fn new(start: u32, len: u32) -> Self {
        List {
            start,
            len,
            item: PhantomData,
        }
    }

    /// How many items the list holds.
    #[must_use]
    pub fn len(self) -> usize {
        self.len as usize
    }

    /// Whether the list holds none.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

impl<T> Default for List<T> {
    /// The empty list.
    fn default() -> Self {
        List::new(0, 0)
    }
}

impl<T> Clone for List<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for List<T> {}

impl<T> PartialEq for List<T> {
    fn eq(&self, other: &Self) -> bool {
        (self.start, self.len) == (other.start, other.len)
    }
}

impl<T> Eq for List<T> {}

impl<T> Hash for List<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self.start, self.len).hash(state);
    }
}

impl<T> fmt::Debug for List<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "List({}..+{})", self.start, self.len)
    }
}

/// What a list may hold: an element type with a pool in [`Lists`].
pub trait Listed: Sized {
    /// The pool of this element type.
    fn pool<'a>(lists: &Lists<'a>) -> &'a [Self];
}

/// The builder's side of [`Listed`]: the pool being filled, and its bit
/// in [`Pools`]' record of the pools written since the last freeze.
pub(crate) trait Pooled: Listed + Copy + 'static {
    /// This pool's bit.
    const BIT: u32;

    /// The pool of this element type while the builder fills it.
    fn pool_mut(pools: &mut Pools) -> &mut Vec<Self>;
}

/// A pool as the tree keeps it: what it holds copied into `bump`, and
/// the pool emptied for the next build. Only a pool a list was written
/// to is frozen ([`Pools::freeze`]), so a tree of a few lists copies
/// those and leaves the rest of [`Lists`] empty.
fn frozen<'a, T: Copy>(bump: &'a ParseArena, pool: &mut Vec<T>) -> &'a [T] {
    let slice = bump.alloc_slice_copy(pool);
    pool.clear();
    slice
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
/// above say, every part of it in the parse's [`crate::Arena`]. An empty
/// tree has no root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tree<'a> {
    /// The shape: every node and significant token in source order, a
    /// node before its subtree.
    elements: &'a [Element],
    /// The arena of nodes.
    nodes: &'a Nodes<'a>,
    /// The lists' pools.
    lists: &'a Lists<'a>,
}

/// The arena of a tree with no node.
const NO_NODES: Nodes<'static> = Nodes {
    slots: [None; SLOTS],
    stores: &[],
};

impl Default for Tree<'_> {
    /// The empty tree.
    fn default() -> Self {
        Tree {
            elements: &[],
            nodes: &NO_NODES,
            lists: &NO_LISTS,
        }
    }
}

impl<'a> Tree<'a> {
    /// The shape, in source order: a node, then the elements inside it.
    #[must_use]
    pub fn elements(&self) -> &'a [Element] {
        self.elements
    }

    /// The outermost node, or `None` for an empty tree.
    #[must_use]
    pub fn root(&self) -> Option<Element> {
        self.elements.first().copied()
    }

    /// Every node of one kind, in the order the builder finished them:
    /// children before their parent, so the root is last.
    #[must_use]
    pub fn nodes<T: Node>(&self) -> &'a [T] {
        T::all(self.nodes)
    }

    /// The items of `list`, as long as the arena lives: a read that
    /// borrows nothing of its reader, so a walk over a list may change
    /// what it holds beside the tree.
    #[must_use]
    pub fn list<T: Listed>(&self, list: List<T>) -> &'a [T] {
        let start = list.start as usize;
        T::pool(self.lists)
            .get(start..start + list.len as usize)
            .unwrap_or_default()
    }
}

impl<T: Node> Index<NodeId<T>> for Tree<'_> {
    type Output = T;

    fn index(&self, id: NodeId<T>) -> &T {
        &T::all(self.nodes)[id.index as usize]
    }
}

impl<T: Listed> Index<List<T>> for Tree<'_> {
    type Output = [T];

    fn index(&self, list: List<T>) -> &[T] {
        self.list(list)
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

/// Where a kind's slice stands in `Nodes::stores`. Held one up in a
/// `NonZeroU8`, so an absent slot, `None`, costs no byte of its own and
/// the slots stay one byte a kind: the slots are copied once, from the
/// builder into the arena, and a wider slot measured 300 instructions
/// more a parse of `return 1;` when they were moved by value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StoreAt(std::num::NonZeroU8);

impl StoreAt {
    /// The place of the `index`-th vector, none past what a byte holds.
    fn new(index: usize) -> Option<StoreAt> {
        let raised = u8::try_from(index.checked_add(1)?).ok()?;
        std::num::NonZeroU8::new(raised).map(StoreAt)
    }

    /// The vector's index in `stores`.
    fn index(self) -> usize {
        usize::from(self.0.get()) - 1
    }
}

impl<'a> Nodes<'a> {
    /// The slice of the struct kind at `slot`, none while no node of the
    /// kind is built.
    fn store(&self, slot: usize) -> Option<&'a Store<'a>> {
        let at = (*self.slots.get(slot)?)?;
        self.stores.get(at.index())
    }
}

/// What the parser counted of its event stream as it wrote it, which is
/// what the builder lays its arena down at: each struct kind's
/// completed nodes by the kind's slot, how many kinds have one, and how
/// many elements the shape holds, a node per completed marker and a
/// token per consumed token. The counts are the builder's stacks' room
/// ([`builder::Stacks`]), a slot per kind at zero between builds:
/// [`Building::open`] takes a kind's count as it lays its vector down,
/// and every kind counted is opened, since every completed node is
/// finished, so a build hands the counts back at zero without a pass
/// over them.
#[derive(Default)]
pub(crate) struct Census {
    counts: MappedVec<u32>,
    kinds: usize,
    shaped: usize,
}

impl Census {
    /// A census over `counts`, the room a parse lends, at zero.
    pub(crate) fn new(mut counts: MappedVec<u32>) -> Census {
        counts.resize(SLOTS, 0);
        Census {
            counts,
            kinds: 0,
            shaped: 0,
        }
    }

    /// The census of a stream written without one: every `Start` the
    /// builder will open and every `Token`.
    pub(crate) fn of(events: &[Event], counts: MappedVec<u32>) -> Census {
        let mut census = Census::new(counts);
        for event in events {
            match *event {
                Event::Start { kind, .. } if kind != TOMBSTONE => census.node(kind),
                Event::Token { .. } => census.token(),
                _ => {}
            }
        }
        census
    }

    /// One node of `kind` completed.
    pub(crate) fn node(&mut self, kind: SyntaxKind) {
        self.shaped += 1;
        if let Some(count) = slot_of(kind).and_then(|slot| self.counts.get_mut(slot)) {
            self.kinds += usize::from(*count == 0);
            *count = count.saturating_add(1);
        }
    }

    /// One token consumed.
    pub(crate) fn token(&mut self) {
        self.shaped += 1;
    }

    /// How many elements the shape holds.
    pub(crate) fn shaped(&self) -> usize {
        self.shaped
    }
}

/// The arena of nodes while the builder fills it: each kind's vector,
/// made with the kind's first node at the count the [`Census`] holds of
/// it, so no vector grows.
pub(crate) struct Building<'a> {
    bump: &'a ParseArena,
    slots: [Option<StoreAt>; SLOTS],
    stores: ArenaVec<'a, Store<'a>>,
    counts: MappedVec<u32>,
}

impl<'a> Building<'a> {
    /// An empty arena in `bump`, to hold what `census` counted.
    pub(crate) fn new(bump: &'a ParseArena, census: Census) -> Building<'a> {
        Building {
            bump,
            slots: [None; SLOTS],
            stores: ArenaVec::with_capacity_in(census.kinds, bump),
            counts: census.counts,
        }
    }

    /// The vector of `T`'s kind, made with the kind's first node. `None`
    /// only where more kinds stand than a slot counts, which the
    /// codegen's count of slots rules out.
    fn open<T: Stored>(&mut self) -> Option<&mut ArenaVec<'a, T>> {
        let at = if let Some(at) = *self.slots.get(T::SLOT)? {
            at
        } else {
            let at = StoreAt::new(self.stores.len())?;
            let room = self.counts.get_mut(T::SLOT).map_or(0, std::mem::take) as usize;
            self.stores
                .push(T::store(ArenaVec::with_capacity_in(room, self.bump)));
            *self.slots.get_mut(T::SLOT)? = Some(at);
            at
        };
        T::stored(self.stores.get_mut(at.index())?)
    }

    /// The arena as the tree keeps it, in the arena, each kind's vector
    /// read where the builder filled it; and the counts back, at zero,
    /// each taken as its kind was opened.
    pub(crate) fn freeze(self) -> (&'a Nodes<'a>, MappedVec<u32>) {
        let Building {
            bump,
            slots,
            stores,
            counts,
        } = self;
        let stores = stores.into_slice();
        (bump.alloc(Nodes { slots, stores }), counts)
    }
}

/// A node as the builder hands it to [`resolve`] at its `Finish`: where it
/// stands, php's line for it, its direct children and the doc comment it
/// took.
pub(crate) struct Finished<'c> {
    pub(crate) range: TextRange,
    pub(crate) line: u32,
    pub(crate) children: &'c [Element],
    pub(crate) doc_comment: Option<TokenId>,
}

/// What a struct node kind is to the arena while the builder fills it:
/// its slot, its vector's variant of `Store`, and its resolution from a
/// finished node, the emitted `resolve` beside its struct.
trait Stored: Sized + 'static {
    /// The kind's slot in `Nodes`.
    const SLOT: usize;
    /// What [`Resolved::Missing`] names where the arena has no vector to
    /// put the kind in.
    const FIELD: &'static str;
    /// The kind's vector as a `Store`.
    fn store(nodes: ArenaVec<'_, Self>) -> Store<'_>;
    /// The kind's vector, if `store` is the kind's.
    fn stored<'a, 'g>(store: &'g mut Store<'a>) -> Option<&'g mut ArenaVec<'a, Self>>;
    /// The node from its children, or the required child that is absent.
    fn resolved(at: &Finished<'_>, pools: &mut Pools) -> Result<Self, &'static str>;
}

/// One finished node of `T`'s kind resolved into its kind's vector, and
/// where it went: the one body every kind's arm of [`resolve`] calls, kept
/// out of line so the dispatch over the kinds is a jump and nothing more.
#[inline(never)]
fn place<T: Stored>(nodes: &mut Building<'_>, pools: &mut Pools, at: &Finished<'_>) -> Resolved {
    let Some(stored) = nodes.open::<T>() else {
        return Resolved::Missing(T::FIELD);
    };
    match T::resolved(at, pools) {
        Ok(node) => {
            // A file the lexer accepts is under `u32::MAX` bytes and a
            // node without a token of its own is bounded by the parser's
            // forward-progress fuel per token, so the count fits; the
            // conversion saturates rather than fails.
            let index = u32::try_from(stored.len()).unwrap_or(u32::MAX);
            stored.push(node);
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

/// Every child the field admits, in order, as a stretch of its pool:
/// counted first, so the pool grows once at most.
fn all<T: Child + Pooled>(pools: &mut Pools, children: &[Element]) -> List<T> {
    listed(pools, children.iter().filter_map(T::of))
}

/// What `items` yields, appended to its pool as one stretch, in one
/// pass: the pools keep their room between builds, so a stretch seldom
/// grows its pool.
fn listed<T: Pooled>(pools: &mut Pools, items: impl Iterator<Item = T>) -> List<T> {
    let pool = T::pool_mut(pools);
    let start = pool.len();
    pool.extend(items);
    let count = pool.len() - start;
    if count == 0 {
        return List::default();
    }
    pools.used |= T::BIT;
    List::new(
        u32::try_from(start).unwrap_or(u32::MAX),
        u32::try_from(count).unwrap_or(u32::MAX),
    )
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
fn tokens(pools: &mut Pools, children: &[Element], kinds: &[SyntaxKind]) -> List<TokenId> {
    listed(
        pools,
        children.iter().filter_map(|element| match element {
            Element::Token { kind, id } if kinds.contains(kind) => Some(*id),
            _ => None,
        }),
    )
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
fn token_range(parsed: &crate::Parsed<'_>, id: TokenId) -> TextRange {
    match parsed.tokens.get(id.0 as usize) {
        Some(token) => TextRange::new(token.offset, token.offset.saturating_add(token.len)),
        None => TextRange::new(parsed.lines.len(), parsed.lines.len()),
    }
}

/// The line a token child starts on, for an alternation's `line`: what the
/// builder gives a token child of a node ([`line`]).
fn token_line(parsed: &crate::Parsed<'_>, id: TokenId) -> u32 {
    parsed.lines.line_number(token_range(parsed, id).start)
}

include!(concat!(env!("OUT_DIR"), "/tree.rs"));
