//! What a date class holds on the request's heap of the timelib structs
//! this crate keeps as Rust values. php builds timelib with its allocator
//! pointed at `emalloc` (`config0.m4`'s `timelib_config.h`), so the
//! `timelib_rel_time` a period holds and the abbreviation
//! a `DateTimeZone` copies are on php's request heap, where
//! `memory_get_usage()` counts them. A `timelib_time` is timelib's own,
//! made on the request's heap under its lend ([`crate::lent`]); the
//! relative time and the abbreviation are copied into Rust values here,
//! and a [`Held`] beside each stands for its blocks at the sizes php asks
//! for (design §5.13).

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Emalloc;

/// `sizeof(timelib_rel_time)`, which `timelib_rel_time_ctor` and
/// `timelib_rel_time_clone` ask for.
pub(crate) const REL_TIME: usize = 104;

/// The one block a struct php keeps on its heap stands for here.
#[derive(Debug)]
pub(crate) struct Held {
    block: Emalloc,
}

/// The size of the block one struct is laid out in.
#[derive(Clone, Copy)]
pub(crate) struct Shape(usize);

impl Shape {
    /// A `timelib_rel_time`.
    pub(crate) fn rel_time() -> Shape {
        Shape(REL_TIME)
    }

    /// `timelib_strdup(abbr)`, a `DateTimeZone`'s `tzi.z.abbr`.
    pub(crate) fn abbr(abbr: &[u8]) -> Shape {
        Shape(abbr.len() + 1)
    }
}

impl Held {
    /// The block brought to `shape`: kept where it has the size asked
    /// for, freed and made again where it has not, and freed for none,
    /// the struct's dtor. The free comes before the allocation, as php's
    /// replacements free before they copy. The `Err` is the memory
    /// limit, what was held then the request's end's.
    pub(crate) fn hold(
        req: &mut Request,
        held: Option<Held>,
        shape: Option<Shape>,
    ) -> Result<Option<Held>, Thrown> {
        let mut kept = None;
        if let Some(held) = held {
            if shape.is_some_and(|s| s.0 == held.block.len()) {
                kept = Some(held.block);
            } else {
                held.block.free(req);
            }
        }
        let Some(shape) = shape else {
            return Ok(None);
        };
        let block = match kept {
            Some(block) => block,
            None => Emalloc::new(req, shape.0)?,
        };
        Ok(Some(Held { block }))
    }

    /// The struct's dtor: its block back to the heap.
    pub(crate) fn free(self, req: &mut Request) {
        self.block.free(req);
    }
}
