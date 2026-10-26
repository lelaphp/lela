//! The arena a parse lives in (ADR-0003, design §5.13, #2810): what
//! dies with a compile, the source's tokens, its line table, the tree's
//! shape, its nodes and their lists, and the compiler's temporaries, is
//! bumped into one [`Arena`] and given up at once when the arena is
//! reset, as php's `CG(ast_arena)` is. Nothing in it has a destructor,
//! so nothing is dropped a node at a time; the unit a compile publishes
//! is an owned value and never lives here.
//!
//! The arena is the caller's, and nothing of it is the thread's: the
//! engine keeps one per request and hands it to every compile the
//! request makes, so the next compile bumps into the room the last one
//! left and a small compile makes no allocation for its tree at all.
//! [`crate::parse`] borrows it and the [`crate::Parsed`] it hands back
//! borrows it in turn, so a parse cannot outlive the memory it is in,
//! and a node is still read by its index in safe code. Beside the bump
//! the arena keeps the parse's stacks, the lexer's stream before it is
//! laid in the arena, the significant-token table, the event stream and
//! the builder's frames, lent to each parse and taken back empty, so
//! their room is reused too.

use std::cell::{Cell, RefCell, RefMut};

use bumpalo::Bump;

use crate::lexer::Token;

/// One arena: memory that is bumped into and given up whole, and the
/// room of the stacks a parse lends itself. A new one, its `default`,
/// allocates nothing until it is first used.
#[derive(Default)]
pub struct Arena {
    bump: Bump,
    /// The stacks [`crate::parse`] lends its stages, borrowed where they
    /// stand for a parse, so a parse nested in another finds them taken
    /// and makes its own.
    scratch: RefCell<Scratch>,
    /// The elements of the last tree parsed into it ([`Arena::parsed`]).
    parsed: Cell<usize>,
}

impl std::fmt::Debug for Arena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Arena")
            .field("allocated", &self.bump.allocated_bytes())
            .finish_non_exhaustive()
    }
}

/// The room [`crate::parse`] lends its stages and takes back empty.
#[derive(Default)]
pub(crate) struct Scratch {
    pub(crate) tokens: Vec<Token>,
    /// The lexer's states above its base.
    pub(crate) states: Vec<crate::lexer::StateFrame>,
    /// The parser's open delimiters.
    pub(crate) delims: Vec<(crate::SyntaxKind, crate::Span)>,
    pub(crate) significant: Vec<u32>,
    pub(crate) stacks: crate::tree::Stacks,
}

impl Scratch {
    /// The most room of one stack an arena keeps between parses: a parse
    /// of a large file pays for its own and gives it up.
    const KEPT: usize = 64 * 1024;

    /// The longest source whose parse grows no stack near
    /// [`Scratch::KEPT`]: every stack holds a few entries per token at
    /// most, and a token is a byte at least, so a parse of a source this
    /// long needs no [`Scratch::trim`] after it.
    pub(crate) const GROWN: usize = 256;

    /// The room kept for the next parse: every stack as it stands where
    /// each is within what an arena keeps, none where one is not.
    pub(crate) fn trim(&mut self) {
        if !self.within(Scratch::KEPT) {
            *self = Scratch::default();
        }
    }

    fn within(&self, bytes: usize) -> bool {
        self.tokens.capacity() * size_of::<Token>() <= bytes
            && self.states.capacity() * size_of::<crate::lexer::StateFrame>() <= bytes
            && self.delims.capacity() * size_of::<(crate::SyntaxKind, crate::Span)>() <= bytes
            && self.significant.capacity() * size_of::<u32>() <= bytes
            && self.stacks.within(bytes)
    }
}

/// The most room the arena keeps when it is reset: a compile of a large
/// file pays for its own and gives it up.
const KEPT: usize = 256 * 1024;

impl Arena {
    /// Give up everything in the arena at once, keeping the room of its
    /// last chunk for what is bumped into it next, up to 256 KiB.
    pub fn reset(&mut self) {
        self.bump.reset();
        if self.bump.allocated_bytes() > KEPT {
            self.bump = Bump::new();
        }
    }

    /// How many elements, nodes and significant tokens, the shape of
    /// the last tree parsed into the arena holds
    /// ([`crate::tree::Tree::elements`]), which a reset keeps.
    #[must_use]
    pub fn parsed(&self) -> usize {
        self.parsed.get()
    }

    /// The shape of a tree parsed into the arena, `elements` long.
    pub(crate) fn note_parsed(&self, elements: usize) {
        self.parsed.set(elements);
    }

    /// The parse's stacks, borrowed where they stand; none while a parse
    /// already holds them.
    pub(crate) fn scratch(&self) -> Option<RefMut<'_, Scratch>> {
        self.scratch.try_borrow_mut().ok()
    }

    /// The bump allocator under the arena, for the builders of this
    /// crate.
    pub(crate) fn bump(&self) -> &Bump {
        &self.bump
    }

    /// A copy of `items` in the arena.
    #[must_use]
    pub fn copy<T: Copy>(&self, items: &[T]) -> &[T] {
        self.bump.alloc_slice_copy(items)
    }

    /// `bytes` with ASCII letters lowercased, as `zend_str_tolower`
    /// folds a name, in the arena.
    #[must_use]
    pub fn lowercase(&self, bytes: &[u8]) -> &[u8] {
        let folded = self.bump.alloc_slice_copy(bytes);
        folded.make_ascii_lowercase();
        folded
    }

    /// An empty vector in the arena with room for `capacity` items. What
    /// it holds is `Copy`, so nothing in the arena is ever owed a drop.
    #[must_use]
    pub fn vec<T: Copy>(&self, capacity: usize) -> ArenaVec<'_, T> {
        ArenaVec(bumpalo::collections::Vec::with_capacity_in(
            capacity, &self.bump,
        ))
    }

    /// `parts` one after another, in the arena.
    #[must_use]
    pub fn concat<T: Copy>(&self, parts: &[&[T]]) -> &[T] {
        let len = parts.iter().map(|part| part.len()).sum();
        let mut joined = bumpalo::collections::Vec::with_capacity_in(len, &self.bump);
        for part in parts {
            joined.extend_from_slice(part);
        }
        joined.into_bump_slice()
    }
}

/// A vector of [`Arena::vec`], grown in the arena and frozen to a slice
/// of it: a compiler's temporary list, given up with the arena.
pub struct ArenaVec<'a, T: Copy>(bumpalo::collections::Vec<'a, T>);

impl<'a, T: Copy> ArenaVec<'a, T> {
    /// `item` at the end.
    pub fn push(&mut self, item: T) {
        self.0.push(item);
    }

    /// `items` at the end.
    pub fn extend_from_slice(&mut self, items: &[T]) {
        self.0.extend_from_slice(items);
    }

    /// What it holds, as a slice of the arena.
    #[must_use]
    pub fn into_slice(self) -> &'a [T] {
        self.0.into_bump_slice()
    }
}

impl<T: Copy> std::ops::Deref for ArenaVec<'_, T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::Arena;

    #[test]
    fn a_reset_arena_keeps_its_room_and_what_it_held_is_given_up() {
        let mut arena = Arena::default();
        assert_eq!(arena.copy(b"abc"), b"abc");
        assert_eq!(arena.concat(&[b"a\\", b"b"]), b"a\\b");
        let mut grown = arena.vec(1);
        grown.push(1);
        grown.extend_from_slice(&[2, 3]);
        assert_eq!(grown.into_slice(), &[1, 2, 3]);
        let room = arena.bump.allocated_bytes();
        assert!(room > 0);
        arena.reset();
        assert_eq!(arena.bump.allocated_bytes(), room);
        let large = vec![0u8; 1024 * 1024];
        assert_eq!(arena.copy(&large).len(), large.len());
        arena.reset();
        assert_eq!(arena.bump.allocated_bytes(), 0, "a large chunk is given up");
    }
}
