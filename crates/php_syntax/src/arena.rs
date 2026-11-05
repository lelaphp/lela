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
//! request makes, a file's bound for the unit cache among them (#4012),
//! so the next compile bumps into the room the last one left and a
//! small compile makes no allocation for its tree at all.
//! [`crate::parse`] borrows it and the [`crate::Parsed`] it hands back
//! borrows it in turn, so a parse cannot outlive the memory it is in,
//! and a node is still read by its index in safe code. Beside the bump
//! the arena keeps the parse's stacks, the lexer's stream before it is
//! laid in the arena, the significant-token table, the event stream and
//! the builder's frames, lent to each parse and taken back empty, and
//! the stacks and the strings of the walk that reads what php's scanner
//! lays, so their room is reused too.
//!
//! The bump and the larger stacks are `php_heap`'s, on mappings of their
//! own, which this crate reaches in safe code ([`ParseArena`],
//! [`MappedVec`]): what a compile grew past [`KEPT`] and
//! [`Scratch::KEPT`] goes back to the machine, where the system
//! allocator would keep the pages dirty in its zone (#4020, #4064).

use std::cell::{Cell, RefCell, RefMut};

pub use php_heap::ArenaVec;
use php_heap::{MappedVec, ParseArena};

use crate::lexer::Token;

/// One arena: memory that is bumped into and given up whole, and the
/// room of the stacks a parse lends itself. A new one, its `default`,
/// allocates nothing until it is first used.
#[derive(Default)]
pub struct Arena {
    bump: ParseArena,
    /// The stacks [`crate::parse`] lends its stages, borrowed where they
    /// stand for a parse, so a parse nested in another finds them taken
    /// and makes its own.
    scratch: RefCell<Scratch>,
    /// The elements of the last tree parsed into it ([`Arena::parsed`]).
    parsed: Cell<usize>,
    /// What php's scanner laid for the last source a compile scanned in
    /// it ([`crate::note_scanner_heap`]).
    scanned: RefCell<crate::ScannerHeap>,
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
    pub(crate) tokens: MappedVec<Token>,
    /// The lexer's states above its base.
    pub(crate) states: Vec<crate::lexer::StateFrame>,
    /// The parser's open delimiters.
    pub(crate) delims: MappedVec<(crate::SyntaxKind, crate::Span)>,
    pub(crate) significant: MappedVec<u32>,
    pub(crate) stacks: crate::tree::Stacks,
    /// The stacks of the walk that lays what php's scanner lays
    /// ([`crate::note_scanner_heap`]).
    pub(crate) walk: crate::scanner::WalkRoom,
}

impl Scratch {
    /// The most room of one stack an arena keeps between parses: a parse
    /// of a large file pays for its own and gives it up. It was read
    /// with the arena's [`KEPT`] on #4064, and a compile that outgrows
    /// either maps again, 234 mappings on Symfony's `about`, a cost ruled
    /// on #4154.
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
            && self.walk.within(bytes)
    }
}

/// The most room the arena keeps when it is reset: the newest chunk,
/// within this. A compile of a large file pays for its own and gives it
/// up. php keeps none and starts each compile at 32 KiB; this was read
/// with [`Scratch::KEPT`] on #4064, and a compile that outgrows either
/// maps again, 234 mappings on Symfony's `about`, a cost ruled on #4154.
const KEPT: usize = 128 * 1024;

impl Arena {
    /// Give up everything in the arena at once, keeping its newest chunk
    /// for what is bumped into it next where it is within [`KEPT`], and
    /// every other chunk back to the machine. It takes the arena by
    /// `&mut`, so no parse is read past the memory it is in:
    ///
    /// ```compile_fail,E0502
    /// let mut arena = php_syntax::Arena::default();
    /// let parsed = php_syntax::parse(&arena, b"<?php", &php_syntax::Options::default());
    /// arena.reset();
    /// assert_eq!(parsed.source, b"<?php");
    /// ```
    pub fn reset(&mut self) {
        self.bump.reset(KEPT);
    }

    /// How many elements, nodes and significant tokens, the shape of
    /// the last tree parsed into the arena holds
    /// ([`crate::tree::Tree::elements`]), which a reset keeps.
    #[must_use]
    pub fn parsed(&self) -> usize {
        self.parsed.get()
    }

    /// What php's scanner laid scanning the source a compile in the
    /// arena read last ([`crate::take_scanner_heap`]), taken out, which a
    /// reset keeps until it is.
    pub(crate) fn take_scanned(&self) -> crate::ScannerHeap {
        self.scanned.take()
    }

    /// What php's scanner lays for the source a compile in the arena
    /// reads, laid by `lay` into the room of the last one and kept for
    /// [`Arena::take_scanned`], over the walk's stacks the arena lends.
    pub(crate) fn lay_scanned(
        &self,
        lay: impl FnOnce(&mut crate::ScannerHeap, &mut crate::scanner::WalkRoom),
    ) {
        let mut heap = self.scanned.take();
        if !heap.within(Scratch::KEPT) {
            heap = crate::ScannerHeap::default();
        }
        let mut lent = self.scratch();
        let mut own = None;
        let room = match lent.as_deref_mut() {
            Some(scratch) => &mut scratch.walk,
            None => own.insert(crate::scanner::WalkRoom::default()),
        };
        lay(&mut heap, room);
        if let Some(scratch) = lent.as_deref_mut() {
            scratch.trim();
        }
        self.scanned.replace(heap);
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
    pub(crate) fn bump(&self) -> &ParseArena {
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
        ArenaVec::with_capacity_in(capacity, &self.bump)
    }

    /// `parts` one after another, in the arena.
    #[must_use]
    pub fn concat<T: Copy>(&self, parts: &[&[T]]) -> &[T] {
        let len = parts.iter().map(|part| part.len()).sum();
        let mut joined = ArenaVec::with_capacity_in(len, &self.bump);
        for part in parts {
            joined.extend_from_slice(part);
        }
        joined.into_slice()
    }
}

#[cfg(test)]
mod tests {
    use super::{Arena, KEPT, Scratch};

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
        let large = vec![0u8; KEPT + 1];
        assert_eq!(arena.copy(&large).len(), large.len());
        arena.reset();
        assert_eq!(arena.bump.allocated_bytes(), 0, "a large chunk is given up");
    }

    #[test]
    fn a_reset_keeps_no_more_than_the_newest_chunk_within_128_kib() {
        let mut arena = Arena::default();
        let mut block = 1024;
        while block <= 4 * KEPT {
            assert_eq!(arena.copy(&vec![0u8; block]).len(), block);
            block *= 2;
            assert!(arena.bump.allocated_bytes() > 0);
            arena.reset();
            let kept = arena.bump.allocated_bytes();
            assert!(kept <= KEPT, "{block}: {kept}");
        }
    }

    /// A parse of a source whose stacks outgrow what an arena keeps
    /// gives all of them back, and the next finds room for its own.
    #[test]
    fn a_large_parse_keeps_no_stack_past_64_kib() {
        let mut arena = Arena::default();
        let source = format!("<?php\n{}", "echo $a + 1;\n".repeat(20_000));
        let source = source.as_bytes();
        let small: &[u8] = b"<?php echo 1;\n";
        {
            let parsed = crate::parse(&arena, source, &crate::Options::default());
            assert!(parsed.diagnostics.is_empty());
            crate::note_scanner_heap(&arena, parsed.source, parsed.tokens);
        }
        assert!(arena.scratch.borrow().within(Scratch::KEPT));
        arena.reset();
        assert!(arena.scratch.borrow().within(Scratch::KEPT));
        assert!(arena.bump.allocated_bytes() <= KEPT);
        let parsed = crate::parse(&arena, small, &crate::Options::default());
        assert!(parsed.diagnostics.is_empty());
    }

    /// What a request does between compiles: a compile that its parser
    /// refuses gives up its arena as one that compiled does, an arena
    /// taken out for a compile leaves an empty one that maps nothing,
    /// and the one put back serves the next.
    #[test]
    fn an_arena_outlives_a_refused_parse_and_a_take_for_a_compile() {
        let mut arena = Arena::default();
        {
            let parsed = crate::parse(&arena, b"<?php function (", &crate::Options::default());
            assert!(parsed.diagnostics.iter().any(|d| d.level.is_fatal()));
        }
        arena.reset();
        let mut taken = std::mem::take(&mut arena);
        assert_eq!(arena.bump.allocated_bytes(), 0);
        {
            let nested = crate::parse(&arena, b"<?php echo 2;", &crate::Options::default());
            let outer = crate::parse(&taken, b"<?php echo 1;", &crate::Options::default());
            assert_eq!(outer.source, b"<?php echo 1;");
            assert_eq!(nested.source, b"<?php echo 2;");
        }
        taken.reset();
        arena = taken;
        let parsed = crate::parse(&arena, b"<?php echo 3;", &crate::Options::default());
        assert!(parsed.diagnostics.is_empty());
    }

    /// One compile's scan in `arena`, as `php_compile` scans a source:
    /// the parse and what php's scanner lays for it, which is handed
    /// back, and the arena reset after.
    fn scan(arena: &mut Arena, source: &[u8]) -> crate::ScannerHeap {
        let laid = {
            let parsed = crate::parse(arena, source, &crate::Options::default());
            crate::note_scanner_heap(arena, parsed.source, parsed.tokens);
            arena.scanned.borrow().clone()
        };
        arena.reset();
        laid
    }

    /// Where each buffer a scan grows stands, and its room.
    fn buffers(arena: &Arena) -> Vec<(usize, usize)> {
        let scratch = arena.scratch.borrow();
        let mut at = scratch.walk.buffers();
        at.extend(arena.scanned.borrow().buffers());
        at.push((scratch.tokens.as_ptr() as usize, scratch.tokens.capacity()));
        at.push((0, arena.bump.allocated_bytes()));
        at
    }

    #[test]
    fn successive_scans_in_one_arena_reuse_its_buffers_and_lay_what_a_fresh_one_does() {
        let first: &[u8] =
            b"<?php\nfunction f($a, $b) { return \"{$a}x\" . 'quoted' . $b->name; }\n\
            $html = <<<EOT\n  text {$x[1]} more\n  EOT;\nclass C { public $p = \"plain\"; }\n\
            echo f(1, 2), \"two words\", `cmd $v`;\n?>trailing html <?php echo 3;";
        let second: &[u8] = b"<?php\n$q = 'other'; echo \"$q and $q\"; function g() { return [1, 2]; }\n\
            interface I { const A = 'a'; } abstract class D implements I { abstract function h(); }\n\
            $long = 'a much longer string than any before'; echo $long, g();\n";
        let alone = |source| scan(&mut Arena::default(), source);
        let mut arena = Arena::default();
        let laid = scan(&mut arena, first);
        assert!(laid.strings().count() > 3, "{laid:?}");
        assert_eq!(laid, alone(first));
        assert_eq!(scan(&mut arena, second), alone(second));
        let grown = buffers(&arena);
        assert!(grown.iter().all(|&(_, room)| room > 0), "{grown:?}");
        assert_eq!(scan(&mut arena, first), laid);
        assert_eq!(scan(&mut arena, second), alone(second));
        assert_eq!(
            buffers(&arena),
            grown,
            "the scans after the first two laid nothing new"
        );
    }
}
