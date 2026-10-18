//! The arena a parse lives in (ADR-0003, design §5.13, #2810): what
//! dies with a compile, the source's tokens, its line table, the tree's
//! shape, its nodes and their lists, and the compiler's temporaries, is
//! bumped into one [`Arena`] and given up at once when the arena is
//! reset, as php's `CG(ast_arena)` is. Nothing in it has a destructor,
//! so nothing is dropped a node at a time; the unit a compile publishes
//! is an owned value and never lives here.
//!
//! The arena is the caller's: [`crate::parse`] borrows one and the
//! [`crate::Parsed`] it hands back borrows it in turn, so a parse cannot
//! outlive the memory it is in, and a node is still read by its index
//! in safe code. [`Arena::lend`] is the per-compile one, the thread's,
//! reset after each use, so a thread that compiles again bumps into the
//! room the last compile left and a small compile makes no allocation
//! for its tree at all.

use std::cell::Cell;

use bumpalo::Bump;

/// One arena: memory that is bumped into and given up whole.
#[derive(Default)]
pub struct Arena {
    bump: Bump,
}

impl std::fmt::Debug for Arena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Arena")
            .field("allocated", &self.bump.allocated_bytes())
            .finish()
    }
}

/// The most room the thread's arena keeps between compiles: a compile of
/// a large file pays for its own and gives it up.
const KEPT: usize = 256 * 1024;

thread_local! {
    static THREADS: Cell<Option<Arena>> = const { Cell::new(None) };
}

impl Arena {
    /// An empty arena, which allocates nothing until it is first used.
    #[must_use]
    pub fn new() -> Arena {
        Arena::default()
    }

    /// `f` over the thread's arena, reset once `f` returns: the
    /// per-compile arena, so what `f` bumped into it is given up whole
    /// and the room is the next compile's. A lend inside another finds
    /// none and makes its own.
    pub fn lend<R>(f: impl FnOnce(&Arena) -> R) -> R {
        let mut arena = THREADS.take().unwrap_or_default();
        let made = f(&arena);
        arena.reset();
        if arena.bump.allocated_bytes() <= KEPT {
            THREADS.set(Some(arena));
        }
        made
    }

    /// Give up everything in the arena at once, keeping the room of its
    /// last chunk for what is bumped into it next.
    fn reset(&mut self) {
        self.bump.reset();
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
    fn a_lent_arena_is_kept_reset_and_a_nested_one_is_its_own() {
        let outer = Arena::lend(|arena| {
            let a = arena.copy(b"abc");
            let nested = Arena::lend(|inner| {
                assert_eq!(inner.copy(b"xyz"), b"xyz");
                std::ptr::from_ref(inner) as usize
            });
            assert_eq!(a, b"abc");
            assert_ne!(nested, std::ptr::from_ref(arena) as usize);
            arena.bump.allocated_bytes()
        });
        assert!(outer > 0);
        Arena::lend(|arena| {
            assert_eq!(arena.concat(&[b"a\\", b"b"]), b"a\\b");
        });
    }
}
