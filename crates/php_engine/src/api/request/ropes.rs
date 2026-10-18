//! The pieces of the interpolated strings a request's frames are
//! building: php's `zend_string *` rope, which `ROPE_INIT` lays over the
//! stretch of temporaries the compiler reserved, two pointers to a zval.
//! A zval here holds one value, so the rope's temporary holds the
//! position of its pieces here instead, and the pieces live in a vector
//! the request keeps between ropes, which no rope allocates once the
//! request has built one as long: what `memory_get_usage()` sees of a
//! rope is its pieces and the string they join into, as php's. A rope belongs to its
//! temporary, not to the order ropes are built in, so a generator that
//! yields and a fiber that suspends mid-rope keep theirs while other
//! ropes are built and ended.

use super::Request;
use crate::api::raising::Thrown;
use crate::api::values::{FreshString, PhpString, Value};

/// A rope's position among the request's, which its temporary holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RopeAt(u32);

impl RopeAt {
    /// The position a temporary's integer names.
    pub(crate) fn of(held: i64) -> Option<RopeAt> {
        u32::try_from(held).ok().map(RopeAt)
    }

    /// The integer the rope's temporary holds.
    pub(crate) fn value(self) -> Value {
        Value::int(i64::from(self.0))
    }
}

/// The ropes being built, each the pieces converted so far, and the
/// positions no rope holds, whose vectors are empty and keep their
/// capacity.
#[derive(Default)]
pub(super) struct Ropes {
    held: Vec<Rope>,
    free: Vec<RopeAt>,
}

/// One position's pieces, and whether a rope holds it: a close of a
/// position no rope holds frees nothing twice.
#[derive(Default)]
struct Rope {
    open: bool,
    pieces: Vec<PhpString>,
}

impl Request {
    /// `ROPE_INIT`'s rope, with no pieces yet. A rope a generator holds
    /// across its `yield`, or a fiber across its suspension, keeps its
    /// pieces while other ropes are built and ended:
    ///
    /// ```php
    /// function g() { $a = [5 => 'five']; $b = 'B'; echo "x$b{$a[yield]}y$b\n"; }
    /// function h() { $q = 'Q'; return "h$q h$q h$q"; }
    /// $g = g(); $g->current(); echo h(), "\n"; $g->send(5);
    /// class S { function __toString(): string { Fiber::suspend(); return 'S'; } }
    /// $f = new Fiber(function () { $s = new S; $p = 'p'; echo "a$p $s b$p\n"; });
    /// $f->start(); echo h(), "\n"; $f->resume();
    /// ```
    /// ```expect
    /// hQ hQ hQ
    /// xBfiveyB
    /// hQ hQ hQ
    /// ap S bp
    /// ```
    pub(crate) fn rope_open(&mut self) -> RopeAt {
        let at = self.ropes.free.pop().unwrap_or_else(|| {
            let at = RopeAt(u32::try_from(self.ropes.held.len()).unwrap_or(u32::MAX));
            self.ropes.held.push(Rope::default());
            at
        });
        if let Some(rope) = self.rope_at(at) {
            rope.open = true;
        }
        at
    }

    /// One more piece onto `rope`, which takes the owner; a rope that is
    /// not open gives the piece up and answers `false`.
    #[inline]
    pub(crate) fn rope_push(&mut self, rope: RopeAt, piece: PhpString) -> bool {
        if let Some(held) = self.rope_at(rope)
            && held.open
        {
            held.pieces.push(piece);
            return true;
        }
        self.release(Value::string(piece));
        false
    }

    /// `zend_string_release_ex` over every piece of `rope`, at its
    /// `ROPE_END` or where an unwind or a generator's destruction meets
    /// it live (`ZEND_LIVE_ROPE`), and its position free again: a
    /// generator destroyed while it waits mid-rope gives its pieces back.
    ///
    /// ```php
    /// function g($n) { $a = [1 => 'one']; $p = str_repeat('p', $n); echo "x$p{$a[yield]}y$p\n"; }
    /// $g = g(50); $g->current(); unset($g);
    /// $m = memory_get_usage(); $g = g(50); $g->current(); unset($g);
    /// echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 0
    /// ```
    pub(crate) fn rope_close(&mut self, rope: RopeAt) {
        let Some(held) = self.rope_at(rope).filter(|held| held.open) else {
            return;
        };
        let pieces = std::mem::take(&mut held.pieces);
        self.rope_give_back(rope, pieces);
    }

    /// Every piece of `pieces`, which `rope` held, given up, the emptied
    /// vector back at its position and the position free.
    fn rope_give_back(&mut self, rope: RopeAt, mut pieces: Vec<PhpString>) {
        for piece in pieces.drain(..) {
            if !piece.heap().is_interned() {
                self.release(Value::string(piece));
            }
        }
        if let Some(held) = self.rope_at(rope) {
            held.open = false;
            held.pieces = pieces;
        }
        self.ropes.free.push(rope);
    }

    /// `ROPE_END`'s join (`Z/Zend/zend_vm_def.h:3510-3530`):
    /// `zend_string_alloc` of the pieces' joined length, each piece's
    /// bytes copied in and the piece released, as php's `memcpy` loop
    /// copies and releases them, and the rope closed. The string is
    /// valid UTF-8 where every piece was known to be,
    /// `ZSTR_COPYABLE_CONCAT_PROPERTIES` narrowed by each piece's. An
    /// allocation refused releases the pieces all the same.
    ///
    /// The rope costs nothing of its own: the peak is the pieces and the
    /// joined string, as php's is.
    ///
    /// ```php
    /// $a = str_repeat('x', 100); $i = 12345;
    /// $s = "a$a b$a"; unset($s);
    /// $m = memory_get_usage(); memory_reset_peak_usage(); $s = "a$a b$a";
    /// echo memory_get_peak_usage() - $m, " ", memory_get_usage() - $m, "\n";
    /// unset($s); $m = memory_get_usage(); memory_reset_peak_usage(); $s = "n$i m$i k$i z";
    /// echo memory_get_peak_usage() - $m, " ", memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 256 288
    /// 144 48
    /// ```
    pub(crate) fn rope_end(&mut self, rope: RopeAt) -> Result<PhpString, Thrown> {
        let pieces = self
            .rope_at(rope)
            .filter(|held| held.open)
            .map(|held| std::mem::take(&mut held.pieces))
            .unwrap_or_default();
        let (len, utf8) = pieces.iter().fold((0_usize, true), |(len, utf8), piece| {
            (
                len.saturating_add(piece.len()),
                utf8 && piece.is_valid_utf8(),
            )
        });
        let joined = FreshString::new(self, len).map(|mut fresh| {
            let bytes = fresh.bytes_mut();
            let mut at: usize = 0;
            for piece in &pieces {
                let end = at.saturating_add(piece.len()).min(bytes.len());
                if let Some(target) = bytes.get_mut(at..end) {
                    target.copy_from_slice(&piece.as_bytes()[..target.len()]);
                }
                at = end;
            }
            let joined = fresh.finish().hand_on();
            if utf8 {
                joined.mark_valid_utf8();
            }
            joined
        });
        if self.rope_at(rope).is_some_and(|held| held.open) {
            self.rope_give_back(rope, pieces);
        }
        joined
    }

    fn rope_at(&mut self, rope: RopeAt) -> Option<&mut Rope> {
        let at = usize::try_from(rope.0).ok()?;
        self.ropes.held.get_mut(at)
    }
}
