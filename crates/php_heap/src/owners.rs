//! A debug build's record of where each owner the engine's owner type
//! holds was made (design §5.1): the side table the owner's
//! `Drop` reads the line that made it from, so the owner itself is its
//! view and nothing more in every build, and a payload whose size is
//! held to php's struct holds one as php holds the `zval`.
//!
//! The table is keyed by the block, as the allocations' origins are
//! ([`crate::sensed::Origin`]), and kept in the block's heap's sensed
//! record, which every chunk's header names, so the owner reaches it
//! from the value it owns with no word of its own: php's `chunk->heap`
//! (`Z/Zend/zend_alloc.c:1454`). Each block names the lines that made
//! the owners of it that stand, in the order they were made; an owner
//! consumed takes the latest of its block's off, so an owner dropped
//! where more than one of its block's stands names the line it was most
//! likely made at and the others beside it.
//!
//! An owner of what no count owns, a scalar, an interned or a persistent
//! string or an immutable array, has no block of a chunk: dropped
//! unconsumed it leaks nothing, so nothing records it. Nor does an owner
//! of a huge block, which heads a mapping of its own with no header to
//! reach the record by; the request leak sensor names that block where
//! the request ends. Nor does any block of a tracked heap, which is the
//! system's (`mm::tracked`). A release build keeps none of this.

use std::collections::HashMap;
use std::panic::Location;
use std::ptr::NonNull;

use crate::Zval;
use crate::mm::{CHUNK_SIZE, freed, tracked};
use crate::sensed::Sensed;

/// The lines that made the owners of one block that stand, in the order
/// they were made: one, which is nearly every block, without a
/// collection beside it.
#[derive(Debug)]
pub(crate) enum Sites {
    One(&'static Location<'static>),
    Many(Vec<&'static Location<'static>>),
}

impl Sites {
    fn push(&mut self, site: &'static Location<'static>) {
        match self {
            Sites::One(first) => *self = Sites::Many(vec![*first, site]),
            Sites::Many(sites) => sites.push(site),
        }
    }

    /// The latest line taken off, and whether any is left.
    fn pop(&mut self) -> (Option<&'static Location<'static>>, bool) {
        match self {
            Sites::One(site) => (Some(*site), false),
            Sites::Many(sites) => {
                let site = sites.pop();
                (site, !sites.is_empty())
            }
        }
    }

    fn lines(&self) -> Vec<&'static Location<'static>> {
        match self {
            Sites::One(site) => vec![*site],
            Sites::Many(sites) => sites.clone(),
        }
    }
}

/// The table: each block an owner stands on, by its address.
pub(crate) type Table = HashMap<usize, Sites>;

/// The block `z` names, where it is a counted block of a chunk.
fn block(z: Zval) -> Option<NonNull<u8>> {
    if !z.is_refcounted() {
        return None;
    }
    if let Some(s) = z.as_str()
        && (s.is_interned() || s.is_persistent())
    {
        return None;
    }
    let at = z.counted()?.block();
    // A tracked heap's blocks are the system's and sit in no chunk.
    let in_chunk = at.as_ptr().addr() & (CHUNK_SIZE - 1) != 0 && !tracked::chosen();
    in_chunk.then_some(at)
}

/// An owner of `z` made at `site`: the engine's owner type as it makes
/// one, and as a write moves one to the copy it separated.
pub fn made(z: Zval, site: &'static Location<'static>) {
    let Some(at) = block(z) else {
        return;
    };
    freed::with_record(at, |record| {
        let mut owners = record.owners();
        match owners.get_mut(&at.as_ptr().addr()) {
            Some(sites) => sites.push(site),
            None => {
                owners.insert(at.as_ptr().addr(), Sites::One(site));
            }
        }
    });
}

/// An owner of `z` consumed, handed to a sink or given up, before its
/// count moves: the latest line that made one of its block's taken off,
/// and answered, for a write that moves the owner to another block.
pub fn consumed(z: Zval) -> Option<&'static Location<'static>> {
    let at = block(z)?;
    freed::with_record(at, |record| take(&mut record.owners(), at).0)
}

/// An owner of `z` dropped without being handed to a sink or given up:
/// recorded for the engine's next question, with the line that made it
/// and the lines of the other owners of its block that stand; nothing
/// while a fatal leaves the request ([`crate::sensed::Record`]).
pub fn dropped(z: Zval) {
    let Some(at) = block(z) else {
        return;
    };
    freed::with_record(at, |record| {
        let (site, others) = take(&mut record.owners(), at);
        if !record.fatal_is_leaving() {
            record.record(Sensed::Unconsumed {
                at: at.as_ptr().addr(),
                site,
                others,
            });
        }
    });
}

/// The latest line of the block at `at` taken off, and the lines of
/// the owners of it left standing.
fn take(
    owners: &mut Table,
    at: NonNull<u8>,
) -> (
    Option<&'static Location<'static>>,
    Vec<&'static Location<'static>>,
) {
    let at = at.as_ptr().addr();
    let Some(sites) = owners.get_mut(&at) else {
        return (None, Vec::new());
    };
    let (site, left) = sites.pop();
    if left {
        return (site, sites.lines());
    }
    owners.remove(&at);
    (site, Vec::new())
}

/// How many owners of the block at `at` stand in the table, for the
/// tests.
#[cfg(test)]
pub(crate) fn standing(at: NonNull<u8>) -> usize {
    freed::with_record(at, |record| {
        record
            .owners()
            .get(&at.as_ptr().addr())
            .map_or(0, |sites| sites.lines().len())
    })
}
