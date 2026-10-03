//! What a date class's timelib structs hold on the request's heap. php
//! builds timelib with its allocator pointed at `emalloc`
//! (`config0.m4`'s `timelib_config.h`: `timelib_calloc` is `ecalloc`,
//! `timelib_strdup` is `estrdup`, `timelib_free` is `efree`), so the
//! `timelib_time` a date or a period holds, the `timelib_rel_time` an
//! interval or a period holds and the abbreviations they copy are on
//! php's request heap, where `memory_get_usage()` counts them. Here the
//! structs are Rust's, and a [`Held`] beside each stands for its blocks
//! at the sizes php asks for (design §5.13).

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Emalloc;

/// `sizeof(timelib_time)`, which `timelib_time_ctor` and
/// `timelib_time_clone` ask `ecalloc` and `emalloc` for.
pub(crate) const TIME: usize = 240;

/// `sizeof(timelib_rel_time)`, which `timelib_rel_time_ctor` and
/// `timelib_rel_time_clone` ask for.
pub(crate) const REL_TIME: usize = 104;

/// What one timelib struct holds: its own block, and the copy of the
/// abbreviation `timelib_strdup` made for it, where it has one.
#[derive(Debug)]
pub(crate) struct Held {
    block: Emalloc,
    abbr: Option<Emalloc>,
}

/// The blocks one struct is laid out in: the struct's size, and the
/// length of the abbreviation it copies, none for none.
#[derive(Clone, Copy)]
pub(crate) struct Shape {
    pub(crate) size: usize,
    pub(crate) abbr: usize,
}

impl Shape {
    /// A `timelib_time` with its `tz_abbr`.
    pub(crate) fn time(abbr: &[u8]) -> Shape {
        Shape {
            size: TIME,
            abbr: abbr.len(),
        }
    }

    /// A `timelib_rel_time`, which copies no abbreviation.
    pub(crate) fn rel_time() -> Shape {
        Shape {
            size: REL_TIME,
            abbr: 0,
        }
    }

    /// `timelib_strdup(abbr)` alone, a `DateTimeZone`'s `tzi.z.abbr`.
    pub(crate) fn abbr(abbr: &[u8]) -> Shape {
        Shape {
            size: abbr.len() + 1,
            abbr: 0,
        }
    }
}

impl Held {
    /// The blocks brought to `shape`: the abbreviation's copy freed and
    /// made again, as `timelib_time_tz_abbr_update` frees and copies it,
    /// the struct's block kept where it has the size asked for and freed
    /// and made again where it has not, and both freed for none, the
    /// struct's dtor. Every free comes before any allocation, as php's
    /// replacements free before they copy. The `Err` is the memory
    /// limit, what was held then the request's end's.
    pub(crate) fn hold(
        req: &mut Request,
        held: Option<Held>,
        shape: Option<Shape>,
    ) -> Result<Option<Held>, Thrown> {
        let mut block = None;
        if let Some(held) = held {
            if let Some(abbr) = held.abbr {
                abbr.free(req);
            }
            if shape.is_some_and(|s| s.size == held.block.len()) {
                block = Some(held.block);
            } else {
                held.block.free(req);
            }
        }
        let Some(shape) = shape else {
            return Ok(None);
        };
        let block = match block {
            Some(block) => block,
            None => Emalloc::new(req, shape.size)?,
        };
        let abbr = if shape.abbr == 0 {
            None
        } else {
            Some(Emalloc::new(req, shape.abbr + 1)?)
        };
        Ok(Some(Held { block, abbr }))
    }

    /// The struct's dtor: its blocks back to the heap.
    pub(crate) fn free(self, req: &mut Request) {
        if let Some(abbr) = self.abbr {
            abbr.free(req);
        }
        self.block.free(req);
    }
}
