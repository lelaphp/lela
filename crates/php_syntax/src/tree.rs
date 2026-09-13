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
//! The nodes, the alternation enums, the mixin traits and the token sets
//! are emitted by `build.rs` into `OUT_DIR/tree.rs` and included below;
//! what is written here by hand is what every emitted item leans on. The
//! arena that owns the nodes and answers a `NodeId`, and the builder that
//! fills the fields, are work of their own.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

use crate::SyntaxKind;
use crate::source::TextRange;

/// The index of one node in the tree's arena, typed by the node it
/// addresses: a `NodeId<ClassDecl>` answers a `ClassDecl` and nothing
/// else, so a field's type says what the child is and the read needs no
/// cast. Four bytes; `Copy`, `Eq` and `Hash` whatever the node, since the
/// node is a phantom.
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

    /// The position in the arena.
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

/// What every emitted node is: a kind, and a range in the source.
pub trait Node {
    /// The node's kind, the one `SyntaxKind` its struct maps to (convention
    /// A1).
    const KIND: SyntaxKind;

    /// Where the node is in the source: from its first significant token
    /// to its last.
    fn range(&self) -> TextRange;
}

include!(concat!(env!("OUT_DIR"), "/tree.rs"));
