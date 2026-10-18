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

use std::cell::Cell;

use bumpalo::Bump;

use crate::lexer::Token;

/// One arena: memory that is bumped into and given up whole, and the
/// room of the stacks a parse lends itself.
#[derive(Default)]
pub struct Arena {
    bump: Bump,
    /// The stacks [`crate::parse`] lends its stages, taken for a parse
    /// and put back after it, so a parse nested in another finds none
    /// and makes its own.
    scratch: Cell<Scratch>,
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
    /// An empty arena, which allocates nothing until it is first used.
    #[must_use]
    pub fn new() -> Arena {
        Arena::default()
    }

    /// Give up everything in the arena at once, keeping the room of its
    /// last chunk for what is bumped into it next, up to 256 KiB.
    pub fn reset(&mut self) {
        self.bump.reset();
        if self.bump.allocated_bytes() > KEPT {
            self.bump = Bump::new();
        }
    }

    /// The parse's stacks, lent until [`Arena::put_back`].
    pub(crate) fn take_scratch(&self) -> Scratch {
        self.scratch.take()
    }

    /// The parse's stacks back, kept where their room is within what an
    /// arena keeps.
    pub(crate) fn put_back(&self, scratch: Scratch) {
        if scratch.within(Scratch::KEPT) {
            self.scratch.set(scratch);
        }
    }

    /// The bump allocator under the arena, for the builders of this
    /// crate.
    pub(crate) fn bump(&self) -> &Bump {
        &self.bump
    }

    /// A copy of `items` in the arena.
    pub(crate) fn copy<T: Copy>(&self, items: &[T]) -> &[T] {
        self.bump.alloc_slice_copy(items)
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

#[cfg(test)]
mod tests {
    use super::Arena;

    #[test]
    fn a_reset_arena_keeps_its_room_and_what_it_held_is_given_up() {
        let mut arena = Arena::new();
        assert_eq!(arena.copy(b"abc"), b"abc");
        assert_eq!(arena.concat(&[b"a\\", b"b"]), b"a\\b");
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
